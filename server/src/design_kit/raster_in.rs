//! PNG / JPEG -> ink coverage -> square binary grid (<= `trace_side`) -> traced path.

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
    let pixels = match format {
        InputFormat::Png => decode_png(bytes, limits)?,
        InputFormat::Jpeg => decode_jpeg(bytes, limits)?,
        InputFormat::Svg => {
            return Err(ProcessError::Internal("SVG passed to the raster pipeline"));
        }
    };
    let (w, h) = (pixels.width, pixels.height);
    let ink = palette::ink(&pixels);
    drop(pixels);

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
    let mask = resample_to_square(&ink.coverage, w, h, side);
    drop(ink);
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

fn decode_png(bytes: &[u8], limits: &Limits) -> Result<Pixels, ProcessError> {
    let side_cap = limits.max_raster_side as usize;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Expand palette/low-bit-depth/tRNS and strip 16-bit to 8-bit gray/GA/RGB/RGBA.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    // Decoder-internal allocation cap (defends against huge iCCP/zTXt chunks etc.).
    decoder.set_limits(png::Limits {
        bytes: side_cap * side_cap * 8 + (1 << 20),
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

fn decode_jpeg(bytes: &[u8], limits: &Limits) -> Result<Pixels, ProcessError> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // Let the header parse succeed for any size so we can report the real dimensions;
    // our own limit is checked before the pixels are decoded.
    let options = DecoderOptions::default()
        .set_max_width(u16::MAX as usize)
        .set_max_height(u16::MAX as usize)
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
    Ok(Pixels {
        width: w,
        height: h,
        channels: 3,
        data,
    })
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
