//! The downloadable design kit in `server/static/design-kit/` (written by
//! `scripts/design-kit/generate.py`, committed) against the studio's template contract:
//! the template colours are exactly `design_kit::palette`, the layer ids the SVG filter
//! looks for exist, the PSD layers carry their own transparency (no masks), and the
//! "Try an example" drawing passes as a head. (The raster and SVG tests use the same
//! committed guide PNGs and SVG template.)

mod common;

use std::collections::BTreeSet;

use arena::design_kit::{
    AssetKind, Limits, Lint, ProcessError, RejectedFormat,
    palette::{self, Rgb},
    process_upload, sniff,
};
use common::design_kit::{design_kit_file, encode_png};
use usvg::roxmltree;

const KINDS: [&str; 2] = ["head", "tail"];

fn hex(c: Rgb) -> String {
    c.to_hex()
}

/// Every `#rrggbb` an element paints with: `fill`, `stroke`, and colours in `style`.
fn paints(node: roxmltree::Node) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["fill", "stroke"] {
        if let Some(v) = node.attribute(attr) {
            out.push(v.to_ascii_lowercase());
        }
    }
    if let Some(style) = node.attribute("style") {
        for decl in style.split(';') {
            if let Some((k, v)) = decl.split_once(':')
                && matches!(k.trim(), "fill" | "stroke" | "color")
            {
                out.push(v.trim().to_ascii_lowercase());
            }
        }
    }
    out
}

/// The colours painted inside `layer` (`none` excluded).
fn layer_colours(layer: roxmltree::Node) -> BTreeSet<String> {
    layer
        .descendants()
        .flat_map(paints)
        .filter(|c| c != "none")
        .collect()
}

#[test]
fn the_svg_templates_use_exactly_the_palette_and_ids() {
    let guides: BTreeSet<String> = palette::GUIDE_COLOURS.into_iter().map(hex).collect();
    let ghost: BTreeSet<String> = [hex(palette::REFERENCE_GHOST)].into();
    assert_eq!(guides.len(), 5, "five distinct guide colours");
    assert!(!guides.contains(&hex(palette::REFERENCE_GHOST)));

    for kind in KINDS {
        let name = format!("battlesnake-{kind}-template.svg");
        let bytes = design_kit_file(&name);
        let text = String::from_utf8(bytes).expect("UTF-8");
        let doc = roxmltree::Document::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let root = doc.root_element();
        assert_eq!(root.attribute("viewBox"), Some("0 0 100 100"), "{name}");

        // The layers are top-level groups.
        let layers: Vec<_> = root
            .children()
            .filter(|n| n.has_tag_name("g"))
            .map(|g| (g.attribute("id").unwrap_or_default().to_string(), g))
            .collect();
        let ids: Vec<&str> = layers.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"draw-here"), "{name}: {ids:?}");
        assert!(ids.contains(&"guides"), "{name}: {ids:?}");
        assert!(
            ids.iter().filter(|id| id.starts_with("reference-")).count() >= 1,
            "{name}: {ids:?}"
        );
        assert!(ids.contains(&"reference-default"), "{name}: {ids:?}");
        assert!(
            ids.iter()
                .all(|id| *id == "draw-here" || *id == "guides" || id.starts_with("reference-")),
            "{name}: unexpected layer in {ids:?}"
        );

        for (id, layer) in &layers {
            let used = layer_colours(*layer);
            match id.as_str() {
                // Empty, filled black: where the artist draws.
                "draw-here" => {
                    assert_eq!(layer.attribute("fill"), Some("#000000"), "{name}");
                    assert_eq!(layer.children().filter(|n| n.is_element()).count(), 0);
                }
                // Every guide colour, and nothing else (no white halo, no black).
                "guides" => {
                    assert_eq!(used, guides, "{name}: guides layer colours");
                    let style = layer.attribute("style").unwrap_or_default();
                    assert!(style.contains("mix-blend-mode:multiply"), "{name}: {style}");
                }
                // The ghost colour only, and hidden until the artist turns it on.
                _ => {
                    assert_eq!(used, ghost, "{name}: {id} colours");
                    let style = layer.attribute("style").unwrap_or_default();
                    assert!(style.contains("display:none"), "{name}: {id} {style}");
                }
            }
        }
    }
}

#[test]
fn an_untouched_svg_template_is_empty_with_the_guides_hint() {
    for kind in KINDS {
        let svg = design_kit_file(&format!("battlesnake-{kind}-template.svg"));
        let e = process_upload(&svg, &Limits::default(), &[]).expect_err("nothing drawn");
        assert_eq!(
            e,
            ProcessError::Empty {
                info: vec![Lint::GuidesVisible]
            },
            "{kind}"
        );
        assert!(
            e.user_message().contains("hide the Guides"),
            "{}",
            e.user_message()
        );
    }
}

#[test]
fn the_psd_templates_are_turned_away_with_advice() {
    for kind in KINDS {
        let psd = design_kit_file(&format!("battlesnake-{kind}-template.psd"));
        assert!(psd.starts_with(b"8BPS"), "{kind}: a PSD");
        assert!(psd.len() < 2 * 1024 * 1024, "{kind}: {} B", psd.len());
        assert_eq!(
            sniff(&psd),
            Err(ProcessError::UnsupportedFormat(RejectedFormat::Psd)),
            "{kind}: uploading the template itself explains what to do"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The PSD templates, read the way an app that imports layers does.
// ---------------------------------------------------------------------------------------

/// A big-endian cursor over a PSD.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        out
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }
    fn i16(&mut self) -> i16 {
        self.u16() as i16
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().expect("4 bytes"))
    }
    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
    /// A section with a u32 length prefix.
    fn section(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        self.take(n)
    }
}

/// One layer: its record, and each channel decoded to one byte per pixel.
struct PsdLayer {
    name: String,
    blend: [u8; 4],
    visible: bool,
    /// The record's layer mask data is non-empty.
    has_mask_data: bool,
    /// Channel id (-1 transparency, 0..=2 RGB, -2/-3 masks) to pixels.
    channels: Vec<(i16, Vec<u8>)>,
    width: usize,
    height: usize,
}

impl PsdLayer {
    fn channel(&self, id: i16) -> &[u8] {
        &self
            .channels
            .iter()
            .find(|(c, _)| *c == id)
            .unwrap_or_else(|| panic!("{}: no channel {id}", self.name))
            .1
    }
}

/// PackBits rows (PSD RLE) to `width * height` bytes.
fn unpack_bits(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut r = Reader { bytes: data, at: 0 };
    let counts: Vec<usize> = (0..height).map(|_| r.u16() as usize).collect();
    let mut out = Vec::with_capacity(width * height);
    for n in counts {
        let row = r.take(n);
        let mut i = 0;
        let start = out.len();
        while i < row.len() {
            let h = row[i] as i8;
            i += 1;
            if h >= 0 {
                let len = h as usize + 1;
                out.extend_from_slice(&row[i..i + len]);
                i += len;
            } else if h != -128 {
                out.extend(std::iter::repeat_n(row[i], (1 - h as isize) as usize));
                i += 1;
            }
        }
        assert_eq!(out.len() - start, width, "row length");
    }
    out
}

/// The header's channel count, the layer count as stored (negative: the composite's
/// first extra channel is its transparency), and the layers, bottom to top.
fn read_psd(bytes: &[u8]) -> (u16, i16, Vec<PsdLayer>) {
    let mut r = Reader { bytes, at: 0 };
    assert_eq!(r.take(4), b"8BPS");
    assert_eq!(r.u16(), 1, "version 1 (PSD, not PSB)");
    r.take(6);
    let channels = r.u16();
    let (_h, _w) = (r.u32(), r.u32());
    assert_eq!(r.u16(), 8, "8 bits per channel");
    assert_eq!(r.u16(), 3, "RGB colour mode");
    r.section(); // colour mode data
    r.section(); // image resources
    let mut lm = Reader {
        bytes: r.section(),
        at: 0,
    };
    let mut li = Reader {
        bytes: lm.section(),
        at: 0,
    };
    let count = li.i16();
    let mut layers = Vec::new();
    let mut lengths = Vec::new();
    for _ in 0..count.unsigned_abs() {
        let (top, left, bottom, right) = (li.i32(), li.i32(), li.i32(), li.i32());
        let n = li.u16();
        let ids: Vec<(i16, usize)> = (0..n).map(|_| (li.i16(), li.u32() as usize)).collect();
        assert_eq!(li.take(4), b"8BIM");
        let blend: [u8; 4] = li.take(4).try_into().expect("blend key");
        let (_opacity, _clipping, flags, _filler) = (li.u8(), li.u8(), li.u8(), li.u8());
        let mut extra = Reader {
            bytes: li.section(),
            at: 0,
        };
        let has_mask_data = !extra.section().is_empty();
        extra.section(); // blending ranges
        let len = extra.u8() as usize;
        let name = String::from_utf8_lossy(extra.take(len)).into_owned();
        layers.push(PsdLayer {
            name,
            blend,
            visible: flags & 0x02 == 0,
            has_mask_data,
            channels: Vec::new(),
            width: (right - left) as usize,
            height: (bottom - top) as usize,
        });
        lengths.push(ids);
    }
    for (layer, ids) in layers.iter_mut().zip(lengths) {
        for (id, len) in ids {
            let data = li.take(len);
            let (w, h) = (layer.width, layer.height);
            let pixels = match u16::from_be_bytes([data[0], data[1]]) {
                0 => data[2..].to_vec(),
                1 => unpack_bits(&data[2..], w, h),
                c => panic!("{}: compression {c}", layer.name),
            };
            assert_eq!(pixels.len(), w * h, "{} channel {id}", layer.name);
            layer.channels.push((id, pixels));
        }
    }
    (channels, count, layers)
}

fn psd_layers(kind: &str) -> Vec<PsdLayer> {
    let (channels, count, layers) = read_psd(&design_kit_file(&format!(
        "battlesnake-{kind}-template.psd"
    )));
    // RGB plus the composite's transparency, as Photoshop writes a layered document.
    assert_eq!(channels, 4, "{kind}");
    assert!(
        count < 0,
        "{kind}: the composite's 4th channel is its transparency"
    );
    layers
}

fn layer<'a>(layers: &'a [PsdLayer], name: &str) -> &'a PsdLayer {
    layers
        .iter()
        .find(|l| l.name == name)
        .unwrap_or_else(|| panic!("no layer {name:?}"))
}

/// Share of `pixels` equal to `v`.
fn share(pixels: &[u8], v: u8) -> f64 {
    pixels.iter().filter(|&&p| p == v).count() as f64 / pixels.len() as f64
}

#[test]
fn the_psd_layers_carry_their_own_transparency() {
    // Each layer's shape is in its transparency channel, with no layer mask. (psd-tools
    // writes the alpha of an RGB document as a mask over solid pixels instead: "Draw
    // here" was solid black under a hide-all mask, so paint on it stayed hidden in apps
    // that keep masks, and apps that drop them opened a black canvas.)
    for kind in KINDS {
        let layers = psd_layers(kind);
        let names: Vec<&str> = layers.iter().rev().map(|l| l.name.as_str()).collect();
        let reference = |slug: &str| format!("Reference: {slug} {kind} (optional)");
        let second = if kind == "head" { "smile" } else { "round-bum" };
        assert_eq!(
            names,
            [
                "Guides (hide before export)",
                "Draw here (black)",
                reference("default").as_str(),
                reference(second).as_str(),
                "Background (leave on)",
            ],
            "{kind}: top to bottom"
        );
        for l in &layers {
            let ids: Vec<i16> = l.channels.iter().map(|(id, _)| *id).collect();
            assert_eq!(ids, [-1, 0, 1, 2], "{kind}: {} channels", l.name);
            assert!(!l.has_mask_data, "{kind}: {} has a layer mask", l.name);
            assert_eq!((l.width, l.height), (1000, 1000), "{kind}: {}", l.name);
        }

        let draw = layer(&layers, "Draw here (black)");
        assert!(draw.visible && &draw.blend == b"norm", "{kind}");
        assert_eq!(
            share(draw.channel(-1), 0),
            1.0,
            "{kind}: Draw here is empty"
        );

        let background = layer(&layers, "Background (leave on)");
        assert!(background.visible, "{kind}");
        for id in [-1, 0, 1, 2] {
            assert_eq!(
                share(background.channel(id), 255),
                1.0,
                "{kind}: white, opaque"
            );
        }

        let guides = layer(&layers, "Guides (hide before export)");
        assert!(
            guides.visible && &guides.blend == b"mul ",
            "{kind}: Multiply"
        );
        let clear = share(guides.channel(-1), 0);
        assert!(
            (0.5..0.95).contains(&clear),
            "{kind}: guides {clear:.3} transparent"
        );

        for slug in ["default", second] {
            let r = layer(&layers, &reference(slug));
            assert!(!r.visible, "{kind}: {slug} reference starts hidden");
            let a = r.channel(-1);
            // Transparent around the shape (the round-bum tail fills 96% of the square),
            // and the light ghost inside it.
            let (clear, solid) = (share(a, 0), share(a, 255));
            assert!(
                clear > 0.02 && solid > 0.05,
                "{kind}: {slug} {clear:.3} {solid:.3}"
            );
            let ghost = palette::REFERENCE_GHOST;
            let (red, green, blue) = (r.channel(0), r.channel(1), r.channel(2));
            for i in (0..a.len()).filter(|&i| a[i] == 255) {
                assert_eq!(
                    Rgb::new(red[i], green[i], blue[i]),
                    ghost,
                    "{kind}: {slug} pixel {i}"
                );
            }
        }
    }
}

/// Composite `layers` (bottom to top; hidden ones skipped, plus `drawing`, black ink
/// coverage, over "Draw here") from their transparency channels, as an app that imports
/// the PSD does, and export an opaque PNG.
fn composite(layers: &[PsdLayer], show: &[&str], drawing: Option<&[u8]>) -> Vec<u8> {
    let n = 1000 * 1000;
    let mut out = vec![[0.0f32; 3]; n];
    for l in layers {
        if !(l.visible || show.contains(&l.name.as_str())) {
            continue;
        }
        let (a, r, g, b) = (l.channel(-1), l.channel(0), l.channel(1), l.channel(2));
        let multiply = &l.blend == b"mul ";
        for (i, px) in out.iter_mut().enumerate() {
            let alpha = a[i] as f32 / 255.0;
            for (c, src) in px.iter_mut().zip([r[i], g[i], b[i]]) {
                let s = src as f32 / 255.0;
                *c = if multiply {
                    *c * (s * alpha + 1.0 - alpha)
                } else {
                    s * alpha + *c * (1.0 - alpha)
                };
            }
        }
        if l.name == "Draw here (black)"
            && let Some(ink) = drawing
        {
            for (px, &k) in out.iter_mut().zip(ink) {
                let alpha = k as f32 / 255.0;
                px.iter_mut().for_each(|c| *c *= 1.0 - alpha);
            }
        }
    }
    let rgb: Vec<u8> = out
        .iter()
        .flat_map(|px| px.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8))
        .collect();
    encode_png(1000, 1000, png::ColorType::Rgb, &rgb)
}

#[test]
fn a_psd_template_exported_with_its_layers_on_is_clean() {
    // The example head (black ink) drawn on "Draw here", exported with the Guides left on
    // and with the default reference turned on too: the studio still sees just the head.
    let example = design_kit_file("example-drawing.png");
    let ink: Vec<u8> = {
        let mut dec = png::Decoder::new(std::io::Cursor::new(&example));
        dec.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = dec.read_info().expect("png info");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("png frame");
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        buf.iter().map(|&v| 255 - v).collect()
    };
    let clean = process_upload(&example, &Limits::default(), &[]).expect("the example");
    let layers = psd_layers("head");
    let reference = "Reference: default head (optional)";
    for show in [&[][..], &[reference][..]] {
        // Guides alone: nothing but the hint.
        let e = process_upload(&composite(&layers, show, None), &Limits::default(), &[])
            .expect_err("guides alone");
        assert_eq!(
            e,
            ProcessError::Empty {
                info: vec![Lint::GuidesVisible]
            },
            "{show:?}"
        );
        // With the drawing: the same head as the clean export.
        let shape = process_upload(
            &composite(&layers, show, Some(&ink)),
            &Limits::default(),
            &[],
        )
        .unwrap_or_else(|e| panic!("{show:?}: {e:?}"));
        assert_eq!(shape.info(), [Lint::GuidesVisible], "{show:?}");
        assert!(
            shape.passes(AssetKind::Head),
            "{show:?}: {:?}",
            shape.lints()
        );
        assert_eq!(shape.metrics().holes, clean.metrics().holes, "{show:?}");
        let fill = (shape.metrics().fill_pct - clean.metrics().fill_pct).abs();
        assert!(fill < 0.5, "{show:?}: fill differs by {fill}");
    }
}

#[test]
fn the_example_drawing_is_a_clean_head() {
    let png = design_kit_file("example-drawing.png");
    assert!(png.len() <= 200 * 1024, "{} B", png.len());
    let shape = process_upload(&png, &Limits::default(), &[]).expect("the example processes");
    assert!(
        shape.lints().head.is_empty(),
        "no head warnings: {:?}",
        shape.lints().head
    );
    assert!(shape.passes(AssetKind::Head));
    assert!(shape.info().is_empty(), "no notes: {:?}", shape.info());
}
