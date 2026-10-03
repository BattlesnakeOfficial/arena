//! PNG / JPEG -> ink coverage -> square binary grid (<= `trace_side`) -> traced path.
//!
//! Memory: a 2048 px RGBA PNG peaks at about 23 MB for the whole process. The decoders
//! are kept near that: PNG metadata chunks we don't use (ICC profiles, text) are skipped
//! rather than inflated, and JPEGs that make zune-jpeg keep every DCT coefficient until
//! the last scan (progressive ones, and baseline ones with one scan per component) get a
//! smaller size cap (see [`buffered_max_pixels`]).

use super::lints::Lint;
use super::palette::{self, Pixels};
use super::{InputFormat, Limits, ProcessError, trace};

/// A traced raster: the path in 0..100 space plus the input facts noticed on the way.
pub(crate) struct Traced {
    pub path: tiny_skia::Path,
    pub info: Vec<Lint>,
}

pub(crate) fn process(
    bytes: &[u8],
    format: InputFormat,
    limits: &Limits,
) -> Result<Traced, ProcessError> {
    // The decoders are third-party code fed untrusted bytes, and zune-jpeg 0.5 panics on
    // some files it accepts (a CMYK JPEG with one scan per component and subsampled
    // colour, at any size). A file the decoder can't handle is the upload's problem: it
    // becomes `InvalidImage` (the panic hook still reports it) rather than unwinding into
    // the caller.
    let decoded = std::panic::catch_unwind(|| match format {
        InputFormat::Png => Ok((decode_png(bytes, limits)?, Orientation::default())),
        InputFormat::Jpeg => decode_jpeg(bytes, limits),
        InputFormat::Svg => Err(ProcessError::Internal("SVG passed to the raster pipeline")),
    });
    let (pixels, orientation) = decoded.unwrap_or_else(|_| {
        Err(ProcessError::InvalidImage(format!(
            "the {} decoder failed on this file",
            format.as_str()
        )))
    })?;
    let ink = palette::ink(&pixels);
    let (stored_w, stored_h) = (pixels.width, pixels.height);
    drop(pixels);
    // Upright, as the artist's preview showed it.
    let (coverage, w, h) = orientation.apply(ink.coverage, stored_w, stored_h);

    let mut info = Vec::new();
    if ink.guides_visible {
        info.push(Lint::GuidesVisible);
    }
    if ink.colours_flattened {
        info.push(Lint::ColoursFlattened);
    }
    if ink.semi_transparent {
        info.push(Lint::SemiTransparent);
    }
    if w != h {
        info.push(Lint::NonSquare {
            width: w as u32,
            height: h as u32,
        });
    }
    if w.max(h) < limits.min_useful_side as usize {
        info.push(Lint::LowResolution {
            width: w as u32,
            height: h as u32,
        });
    }

    // Square trace grid: the canvas maps uniformly (centred) onto the 100x100 box.
    // Big images are box-downscaled to `trace_side`; small ones bilinearly upscaled to
    // >= 512 px so the splines come out smooth.
    let side = w.max(h).clamp(512, (limits.trace_side as usize).max(512));
    let mask = resample_to_square(&coverage, w, h, side);
    drop(coverage);
    let traced = trace::trace_mask(mask, side, limits)?;
    if traced.specks_removed > 0 {
        info.push(Lint::SpecksRemoved {
            count: traced.specks_removed,
        });
    }
    let Some(path) = traced.path else {
        return Err(ProcessError::Empty { info });
    };
    Ok(Traced { path, info })
}

/// The PNG decoder's own allocation cap: a few 16-bit RGBA rows at 2048 px plus the
/// small chunks it still parses.
const PNG_DECODER_BYTES: usize = 4 << 20;

fn decode_png(bytes: &[u8], limits: &Limits) -> Result<Pixels, ProcessError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Expand palette/low-bit-depth/tRNS and strip 16-bit to 8-bit gray/GA/RGB/RGBA.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    // We never use colour profiles or text, and a compressed iCCP or zTXt chunk would
    // otherwise be inflated in full.
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    // Decoder-internal allocations only (row buffers and the chunks it still reads); the
    // frame buffer below is ours and is sized from the checked header.
    decoder.set_limits(png::Limits {
        bytes: PNG_DECODER_BYTES,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|e| ProcessError::InvalidImage(e.to_string()))?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    // Header dimensions are checked BEFORE the frame buffer is allocated.
    check_dimensions(w, h, limits)?;
    let size = reader
        .output_buffer_size()
        .ok_or(ProcessError::ImageTooLarge {
            width: w,
            height: h,
            max_side: limits.max_raster_side,
        })?;
    let mut buf = vec![0u8; size];
    // Only the first frame of an APNG is used.
    let out = reader
        .next_frame(&mut buf)
        .map_err(|e| ProcessError::InvalidImage(e.to_string()))?;
    let channels = match out.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(ProcessError::Internal("PNG palette was not expanded"));
        }
    };
    if out.bit_depth != png::BitDepth::Eight {
        return Err(ProcessError::Internal(
            "PNG was not normalised to 8 bits per channel",
        ));
    }
    let (w, h) = (out.width as usize, out.height as usize);
    let row = w * channels;
    let data = if out.line_size == row {
        buf.truncate(row * h);
        buf
    } else {
        let mut packed = Vec::with_capacity(row * h);
        for y in 0..h {
            let start = y * out.line_size;
            let line = buf
                .get(start..start + row)
                .ok_or(ProcessError::Internal("PNG row out of bounds"))?;
            packed.extend_from_slice(line);
        }
        packed
    };
    Ok(Pixels {
        width: w,
        height: h,
        channels,
        data,
    })
}

fn decode_jpeg(bytes: &[u8], limits: &Limits) -> Result<(Pixels, Orientation), ProcessError> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // Let the header parse succeed for any size so we can report the real dimensions;
    // our own limit is checked before the pixels are decoded. Strict mode turns a
    // truncated or corrupt scan into an error instead of grey filler rows.
    let options = DecoderOptions::default()
        .set_max_width(u16::MAX as usize)
        .set_max_height(u16::MAX as usize)
        .set_strict_mode(true)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|e| ProcessError::InvalidImage(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or(ProcessError::Internal("JPEG headers decoded without info"))?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    check_dimensions(w, h, limits)?;
    // zune-jpeg decodes a scan straight to pixels only when it is sequential and its first
    // scan carries every component. Otherwise (progressive, or one scan per component,
    // which encoders write when they optimise their Huffman tables) it keeps the whole
    // image's coefficients until the last scan.
    let buffers_coefficients =
        !info.sof.is_sequential_dct() || first_scan_components(bytes) != Some(info.components);
    if buffers_coefficients {
        let max = buffered_max_pixels(limits, info.components);
        if w as usize * h as usize > max {
            return Err(ProcessError::ImageTooLarge {
                width: w,
                height: h,
                max_side: max.isqrt() as u32,
            });
        }
    }
    let orientation = decoder
        .exif()
        .and_then(|tiff| Orientation::from_exif(tiff))
        .unwrap_or_default();
    let data = decoder
        .decode()
        .map_err(|e| ProcessError::InvalidImage(e.to_string()))?;
    let (w, h) = (w as usize, h as usize);
    if data.len() != w * h * 3 {
        return Err(ProcessError::InvalidImage(format!(
            "decoded {} bytes for a {w}x{h} RGB image",
            data.len()
        )));
    }
    let pixels = Pixels {
        width: w,
        height: h,
        channels: 3,
        data,
    };
    Ok((pixels, orientation))
}

/// Most pixels in a JPEG whose coefficients zune-jpeg buffers (progressive, or not
/// interleaved). It keeps a 2-byte coefficient per sample until the last scan, on top of
/// the RGB output, so a 2048 px RGB one would need about 40 MB (47 MB for CMYK). This
/// holds the coefficients to the size of the largest RGB output we accept
/// (`max_raster_side`² × 3 bytes): 1448 px square for 3 components, 1254 for 4, the full
/// 2048 for greyscale.
fn buffered_max_pixels(limits: &Limits, components: u8) -> usize {
    let side = limits.max_raster_side as usize;
    side * side * 3 / (2 * usize::from(components.max(1)))
}

/// `Ns` of a JPEG's first scan header (SOS): how many components its first scan carries.
/// `None` when there is no scan header or it is cut off.
///
/// Walks the markers the way zune-jpeg 0.5 does, so both find the same scan: every
/// segment before the first SOS is skipped by its declared length (zune-jpeg's parsers
/// consume exactly that or fail), `0xFF` and `0x00` fill bytes after an `0xFF` are
/// skipped, and other stray bytes are ignored.
fn first_scan_components(bytes: &[u8]) -> Option<u8> {
    if bytes.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut i = 2usize;
    let mut last = 0u8;
    loop {
        let mut m = *bytes.get(i)?;
        i += 1;
        if (m == 0xff || m == 0) && last == 0xff {
            while m == 0xff || m == 0 {
                last = m;
                m = *bytes.get(i)?;
                i += 1;
            }
        }
        if last == 0xff {
            match m {
                // SOS: length (2 bytes), then Ns.
                0xda => return bytes.get(i.checked_add(2)?).copied(),
                // EOI before any scan.
                0xd9 => return None,
                _ => {
                    let len = u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]);
                    if len < 2 {
                        return None;
                    }
                    i = i.checked_add(usize::from(len))?;
                }
            }
        }
        last = m;
    }
}

/// EXIF orientation: how the stored pixels must be turned to show the image upright.
/// Browsers apply it to the artist's preview (cameras store landscape pixels with
/// "rotate 90°", and apps that crop or resize a photo often keep the tag), so we apply it
/// too. Camera originals themselves are larger than `max_raster_side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Orientation(u8);

impl Default for Orientation {
    fn default() -> Self {
        Orientation(1)
    }
}

impl Orientation {
    /// Tag 0x0112 of IFD0 in a TIFF-structured EXIF block (what follows `Exif\0\0`).
    /// `None` when it's missing, malformed or out of range.
    pub(crate) fn from_exif(tiff: &[u8]) -> Option<Orientation> {
        let little = match tiff.get(0..4)? {
            b"II*\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        let u16_at = |i: usize| {
            let b: [u8; 2] = tiff.get(i..i.checked_add(2)?)?.try_into().ok()?;
            Some(if little {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            })
        };
        let u32_at = |i: usize| {
            let b: [u8; 4] = tiff.get(i..i.checked_add(4)?)?.try_into().ok()?;
            Some(if little {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            })
        };
        let ifd = usize::try_from(u32_at(4)?).ok()?;
        let entries = usize::from(u16_at(ifd)?);
        (0..entries).find_map(|n| {
            let entry = ifd.checked_add(2 + n * 12)?;
            // Tag 0x0112, type SHORT: the value is in the first 2 bytes of the value field.
            if u16_at(entry)? != 0x0112 || u16_at(entry + 2)? != 3 {
                return None;
            }
            let v = u8::try_from(u16_at(entry + 8)?).ok()?;
            (1..=8).contains(&v).then_some(Orientation(v))
        })
    }

    /// Turn a `w`x`h` row-major buffer upright. Returns it with its new width and height.
    pub(crate) fn apply(self, src: Vec<u8>, w: usize, h: usize) -> (Vec<u8>, usize, usize) {
        if self.0 == 1 || src.len() != w * h {
            return (src, w, h);
        }
        // 5-8 are transposed (a quarter turn, with or without a mirror).
        let (dw, dh) = if self.0 >= 5 { (h, w) } else { (w, h) };
        let mut out = vec![0u8; src.len()];
        for dy in 0..dh {
            for dx in 0..dw {
                let (sx, sy) = match self.0 {
                    2 => (w - 1 - dx, dy),
                    3 => (w - 1 - dx, h - 1 - dy),
                    4 => (dx, h - 1 - dy),
                    5 => (dy, dx),
                    6 => (dy, h - 1 - dx),
                    7 => (w - 1 - dy, h - 1 - dx),
                    8 => (w - 1 - dy, dx),
                    _ => (dx, dy),
                };
                out[dy * dw + dx] = src[sy * w + sx];
            }
        }
        (out, dw, dh)
    }
}

fn check_dimensions(w: u32, h: u32, limits: &Limits) -> Result<(), ProcessError> {
    if w == 0 || h == 0 {
        return Err(ProcessError::InvalidImage(format!(
            "image has no pixels ({w}x{h})"
        )));
    }
    if w > limits.max_raster_side || h > limits.max_raster_side {
        return Err(ProcessError::ImageTooLarge {
            width: w,
            height: h,
            max_side: limits.max_raster_side,
        });
    }
    Ok(())
}

/// Map a `w`x`h` coverage image into a `side`x`side` boolean grid, uniformly scaled and
/// centred. Downscale = box average, upscale = bilinear. Threshold at 50%.
fn resample_to_square(ink: &[u8], w: usize, h: usize, side: usize) -> Vec<bool> {
    let src_side = w.max(h) as f64;
    let k = side as f64 / src_side; // dst px per src px
    let off_x = (side as f64 - w as f64 * k) / 2.0;
    let off_y = (side as f64 - h as f64 * k) / 2.0;
    let mut out = vec![false; side * side];
    let get = |x: usize, y: usize| -> f64 { ink[y * w + x] as f64 };
    for ty in 0..side {
        let sy0 = (ty as f64 - off_y) / k;
        let sy1 = (ty as f64 + 1.0 - off_y) / k;
        if sy1 <= 0.0 || sy0 >= h as f64 {
            continue;
        }
        for tx in 0..side {
            // Source-space rect covered by this target pixel.
            let sx0 = (tx as f64 - off_x) / k;
            let sx1 = (tx as f64 + 1.0 - off_x) / k;
            if sx1 <= 0.0 || sx0 >= w as f64 {
                continue;
            }
            let v = if k < 1.0 {
                let (x0, x1) = (sx0.floor().max(0.0) as usize, (sx1.ceil() as usize).min(w));
                let (y0, y1) = (sy0.floor().max(0.0) as usize, (sy1.ceil() as usize).min(h));
                let mut sum = 0.0;
                let mut n = 0.0;
                for y in y0..y1 {
                    for x in x0..x1 {
                        sum += get(x, y);
                        n += 1.0;
                    }
                }
                if n > 0.0 { sum / n } else { 0.0 }
            } else {
                let cx = (sx0 + sx1) / 2.0 - 0.5;
                let cy = (sy0 + sy1) / 2.0 - 0.5;
                let (fx0, fy0) = (cx.floor(), cy.floor());
                let (fx, fy) = (cx - fx0, cy - fy0);
                let clamp_x = |x: f64| x.clamp(0.0, (w - 1) as f64) as usize;
                let clamp_y = |y: f64| y.clamp(0.0, (h - 1) as f64) as usize;
                let (x0, x1) = (clamp_x(fx0), clamp_x(fx0 + 1.0));
                let (y0, y1) = (clamp_y(fy0), clamp_y(fy0 + 1.0));
                get(x0, y0) * (1.0 - fx) * (1.0 - fy)
                    + get(x1, y0) * fx * (1.0 - fy)
                    + get(x0, y1) * (1.0 - fx) * fy
                    + get(x1, y1) * fx * fy
            };
            out[ty * side + tx] = v >= 127.5;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 image, stored the way EXIF `orientation` describes, read back upright.
    fn upright(orientation: u8) -> (Vec<u8>, usize, usize) {
        // Upright it reads 1 2 3 / 4 5 6. Store it as the camera would.
        let stored: (Vec<u8>, usize, usize) = match orientation {
            1 => (vec![1, 2, 3, 4, 5, 6], 3, 2),
            2 => (vec![3, 2, 1, 6, 5, 4], 3, 2),
            3 => (vec![6, 5, 4, 3, 2, 1], 3, 2),
            4 => (vec![4, 5, 6, 1, 2, 3], 3, 2),
            5 => (vec![1, 4, 2, 5, 3, 6], 2, 3),
            // "Rotate 90° clockwise to view": the stored left column is the top row.
            6 => (vec![3, 6, 2, 5, 1, 4], 2, 3),
            7 => (vec![6, 3, 5, 2, 4, 1], 2, 3),
            _ => (vec![4, 1, 5, 2, 6, 3], 2, 3),
        };
        Orientation(orientation).apply(stored.0, stored.1, stored.2)
    }

    #[test]
    fn every_exif_orientation_reads_upright() {
        for o in 1..=8 {
            assert_eq!(
                upright(o),
                (vec![1, 2, 3, 4, 5, 6], 3, 2),
                "orientation {o}"
            );
        }
    }

    #[test]
    fn exif_orientation_is_parsed_from_either_byte_order() {
        // Little-endian TIFF header, IFD0 at 8 with two entries; orientation is second.
        let mut le = b"II*\0\x08\0\0\0\x02\0".to_vec();
        le.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0]); // Make, ASCII
        le.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(Orientation::from_exif(&le), Some(Orientation(6)));
        let mut be = b"MM\0*\0\0\0\x08\0\x01".to_vec();
        be.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 8, 0, 0]);
        assert_eq!(Orientation::from_exif(&be), Some(Orientation(8)));
        // Out of range, truncated, or not TIFF.
        let mut bad = le.clone();
        bad[30] = 9;
        assert_eq!(Orientation::from_exif(&bad), None);
        assert_eq!(Orientation::from_exif(&le[..25]), None);
        assert_eq!(Orientation::from_exif(b"JFIF"), None);
        // An IFD offset or entry count pointing past the end doesn't panic.
        assert_eq!(Orientation::from_exif(b"II*\0\xff\xff\xff\xff"), None);
        assert_eq!(Orientation::from_exif(b"II*\0\x08\0\0\0\xff\xff"), None);
    }

    #[test]
    fn the_first_scan_is_found_the_way_zune_jpeg_finds_it() {
        let sos = |ns: u8| {
            let mut v = vec![0xff, 0xda, 0, 6 + 2 * ns, ns];
            v.extend(std::iter::repeat_n(0, 2 * usize::from(ns) + 3));
            v
        };
        let jpeg = |segments: &[&[u8]]| {
            let mut v = vec![0xff, 0xd8];
            for s in segments {
                v.extend_from_slice(s);
            }
            v
        };
        // A DQT-like segment, then a 3-component scan.
        let dqt: &[u8] = &[0xff, 0xdb, 0, 4, 0xaa, 0xbb];
        assert_eq!(first_scan_components(&jpeg(&[dqt, &sos(3)])), Some(3));
        // A scan header inside a segment (an EXIF thumbnail) is skipped with it.
        let app1: &[u8] = &[0xff, 0xe1, 0, 9, 0xff, 0xda, 0, 8, 1, 0, 0];
        assert_eq!(first_scan_components(&jpeg(&[app1, &sos(1)])), Some(1));
        // Fill bytes before a marker, and a few stray bytes between segments.
        assert_eq!(
            first_scan_components(&jpeg(&[&[0xff, 0xff, 0xff], dqt, &[1, 2], &sos(4)])),
            Some(4)
        );
        // 0xFF 0x00 is not a marker: the SOS after it is ignored, like zune-jpeg does.
        assert_eq!(
            first_scan_components(&jpeg(&[dqt, &[0xff, 0x00, 0xda, 0, 8, 1]])),
            None
        );
        // No SOI, an EOI first, a bad length, or cut off.
        assert_eq!(first_scan_components(&sos(3)), None);
        assert_eq!(
            first_scan_components(&jpeg(&[&[0xff, 0xd9], &sos(3)])),
            None
        );
        assert_eq!(first_scan_components(&jpeg(&[&[0xff, 0xdb, 0, 1]])), None);
        assert_eq!(first_scan_components(&jpeg(&[&sos(3)[..4]])), None);
        assert_eq!(
            first_scan_components(&[0xff, 0xd8, 0xff, 0xdb, 0xff, 0xff]),
            None
        );
    }

    #[test]
    fn resample_centres_a_wide_canvas() {
        // 4x2 all-ink canvas -> rows 0..side/4 and 3/4..side are empty.
        let mask = resample_to_square(&[255; 8], 4, 2, 512);
        assert!(!mask[10 * 512 + 256]);
        assert!(mask[256 * 512 + 256]);
        assert!(!mask[500 * 512 + 256]);
        assert!(mask[129 * 512] && mask[382 * 512 + 511]);
    }
}
