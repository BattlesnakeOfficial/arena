#!/usr/bin/env python3
"""Generate the Battlesnake head & tail design-kit templates (DEV-1539).

Deterministic, offline. Inputs (vendored next to this script):
  vendor/fonts/DejaVuSans-Bold.ttf      label glyphs, outlined to paths
  vendor/heads/<slug>.svg               reference heads (from media.battlesnake.com)
  vendor/tails/<slug>.svg               reference tails

Outputs (server/static/design-kit/ in the arena repo, else out/; override with --out):
  battlesnake-{head,tail}-template.psd   1000x1000 layered PSD (Procreate Gallery -> Import)
  battlesnake-{head,tail}-template.svg   viewBox 0 0 100 100, Inkscape-style layers
  battlesnake-{head,tail}-guide.png      guides only, transparent, 1000x1000
  example-drawing.png                    hand-drawn-looking head for "Try an example"

Single source of truth: every layer is authored once as an SVG fragment in the
100x100 board coordinate space; the SVG template embeds the fragments, the PSD
rasterises each fragment with cairosvg into its own pixel layer.

Usage:  python generate.py [--out DIR] [--previews]
"""

from __future__ import annotations

import argparse
import io
import re
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path

import cairosvg
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from PIL import Image
from psd_tools import PSDImage
from psd_tools.constants import BlendMode, Compression, ProtectedFlags
from psd_tools.constants import Tag

import example_drawing

HERE = Path(__file__).resolve().parent
# In the arena repo this script lives in scripts/design-kit/ and writes straight
# into the committed static dir; anywhere else it writes to ./out.
REPO_STATIC = HERE.parents[1] / "server" / "static"
DEFAULT_OUT = REPO_STATIC / "design-kit" if REPO_STATIC.is_dir() else HERE / "out"
PREVIEWS = HERE / "previews"
CANVAS_PX = 1000  # 10 px per board unit
UNIT = 100  # SVG viewBox size

# Template contract palette. Every guide colour is light: its luma is above 0.5,
# so a guide pixel is never ink by itself, and the studio's "near a template
# colour" exclusion around it never reaches the dark inks artists draw with
# (navy, sapphire, royal blue, crimson, raspberry...). The text colours are
# the darkest of them, chosen for about 3.2:1 contrast on white (WCAG's bar
# for large text); the line colours are lighter still. They're saturated hues
# so the studio can tell them from the grey anti-aliasing of black ink.
#
# CONTRACT: these six values must equal `design_kit::palette` in the server
# (a Rust test parses the committed SVG templates and compares). Every fill and
# stroke inside the `guides` and `reference-*` layers uses one of them and
# nothing else (no white halos, no black), so the studio can drop guide
# elements by colour even after an app such as Figma strips the layer ids.
# `check_contract()` enforces this on every run.
GRID = "#bfe3f7"  # grid lines, vertical centre line
GUIDE = "#9eb0f4"  # canvas border, the "middle" spine line
LABEL = "#7e8fb8"  # all text except the neck label, the arrow, size swatches
ATTACH = "#ff94b8"  # the neck/attach edge: band, edge strip, arrows
ATTACH_TEXT = "#b4808c"  # the "neck attaches here" label
# Reference tint is baked into the pixels (not layer opacity) so it stays a light
# "ghost" even in an app that ignores PSD opacity/visibility flags, and so the
# studio's dark-pixel ink test never mistakes it for ink.
REF_FILL = "#c8c2d4"
PALETTE = (GRID, GUIDE, LABEL, ATTACH, ATTACH_TEXT, REF_FILL)

# Layer ids in the SVG template (also part of the contract).
DRAW_ID = "draw-here"
GUIDES_ID = "guides"
REF_ID_PREFIX = "reference-"

OUTPUT_PREFIX = "battlesnake"

FONT = TTFont(HERE / "vendor/fonts/DejaVuSans-Bold.ttf")
GLYPHS = FONT.getGlyphSet()
CMAP = FONT.getBestCmap()
UPM = FONT["head"].unitsPerEm


# --------------------------------------------------------------------------
# Text -> outlined path (so no app ever shows a "missing fonts" dialog, and the
# SVG and PSD render identically everywhere).
# --------------------------------------------------------------------------
def text_width(text: str, size: float) -> float:
    hmtx = FONT["hmtx"]
    return sum(hmtx[CMAP.get(ord(c), ".notdef")][0] for c in text) * size / UPM


def text_path(text: str, x: float, y: float, size: float, anchor: str = "start") -> str:
    """Return an SVG path `d` for `text` with its baseline at y."""
    if anchor == "middle":
        x -= text_width(text, size) / 2
    elif anchor == "end":
        x -= text_width(text, size)
    pen = SVGPathPen(GLYPHS, ntos=lambda v: f"{v:.2f}".rstrip("0").rstrip("."))
    s = size / UPM
    cursor = 0.0
    hmtx = FONT["hmtx"]
    for ch in text:
        name = CMAP.get(ord(ch), ".notdef")
        # y-flip: font units are y-up, SVG is y-down.
        tp = TransformPen(pen, (s, 0, 0, -s, x + cursor * s, y))
        GLYPHS[name].draw(tp)
        cursor += hmtx[name][0]
    return pen.getCommands()


def label(text: str, x: float, y: float, size: float = 2.6, anchor: str = "start",
          fill: str = LABEL, transform: str | None = None) -> str:
    tr = f' transform="{transform}"' if transform else ""
    # No white halo: it would be a non-palette colour, and in an app that drops
    # the Multiply blend (or an SVG with its ids stripped) a white halo over the
    # drawing reads as a hole. The grid is light enough that labels stay legible.
    d = text_path(text, x, y, size, anchor)
    return f'<path d="{d}" fill="{fill}"{tr}/>'


# --------------------------------------------------------------------------
# Layer fragments (100x100 coordinate space)
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class Kind:
    key: str  # "head" | "tail"
    title: str
    attach_label: str
    direction_label: str
    references: tuple[str, ...]


HEAD = Kind("head", "HEAD", "neck attaches here (full height)", "faces right",
            ("default", "smile"))
TAIL = Kind("tail", "TAIL", "body attaches here (full height)", "tip points right",
            ("default", "round-bum"))


def guides_fragment(kind: Kind) -> str:
    parts: list[str] = []
    # Grid every 10 units (100 px).
    lines = []
    for i in range(10, 100, 10):
        lines.append(f"M{i} 0V100M0 {i}H100")
    parts.append(f'<path d="{"".join(lines)}" stroke="{GRID}" stroke-width="0.2" fill="none"/>')
    # Canvas border so the cell edge is obvious in apps with a grey pasteboard.
    parts.append(f'<rect x="0.15" y="0.15" width="99.7" height="99.7" fill="none" '
                 f'stroke="{GUIDE}" stroke-width="0.3"/>')
    # Centre lines: the spine (horizontal) is the one that matters.
    parts.append(f'<path d="M0 50H100" stroke="{GUIDE}" stroke-width="0.4" '
                 f'stroke-dasharray="2 1.2" fill="none"/>')
    parts.append(f'<path d="M50 0V100" stroke="{GRID}" stroke-width="0.3" '
                 f'stroke-dasharray="1 1" fill="none"/>')
    parts.append(label("middle", 98, 48.5, 2.2, "end"))

    # Attach edge: tinted band + solid edge + ticks pointing at the edge.
    parts.append(f'<rect x="0" y="0" width="6" height="100" fill="{ATTACH}" opacity="0.25"/>')
    parts.append(f'<rect x="0" y="0" width="1.2" height="100" fill="{ATTACH}"/>')
    ticks = "".join(f"M1.6 {y}l4 -2v4z" for y in (12, 88))
    parts.append(f'<path d="{ticks}" fill="{ATTACH}"/>')
    parts.append(label(kind.attach_label, -50, 4.5, 2.7, "middle", ATTACH_TEXT,
                       transform="rotate(-90)"))

    # Direction arrow, top right.
    parts.append(label(kind.direction_label, 89.5, 7.6, 3.0, "end"))
    parts.append(f'<path d="M90.6 6.6H96" stroke="{LABEL}" stroke-width="0.7" fill="none"/>'
                 f'<path d="M97.6 6.6l-2.8 -1.9v3.8z" fill="{LABEL}"/>')

    # Title, top left (just inside the attach band). The text is all one colour,
    # the template's most legible one; the lines are lighter.
    parts.append(label(f"{kind.title} TEMPLATE", 8.5, 5.8, 2.8))
    parts.append(label("1000 \u00d7 1000 px \u00b7 grid squares are 100 px", 8.5, 9.0, 2.1))
    parts.append(label("mirrored when moving left, so skip lettering", 8.5, 11.9, 2.1))

    # Minimum detail size swatches, bottom right: 4 units = 40 px.
    sx, sy = 74.5, 90.3
    parts.append(label("keep details \u2265 40 px", 97.5, 87.4, 2.4, "end"))
    parts.append(f'<circle cx="{sx + 2}" cy="{sy + 2}" r="2" fill="{LABEL}"/>')
    parts.append(f'<rect x="{sx + 6}" y="{sy}" width="4" height="4" fill="{LABEL}"/>')
    # a 4-unit gap between two bars: holes need room too
    parts.append(f'<rect x="{sx + 13}" y="{sy}" width="1.6" height="4" fill="{LABEL}"/>'
                 f'<rect x="{sx + 18.6}" y="{sy}" width="1.6" height="4" fill="{LABEL}"/>')
    for cx, word in ((sx + 2, "dot"), (sx + 8, "block"), (sx + 16.6, "gap")):
        parts.append(label(word, cx, sy + 7.1, 1.8, "middle"))
    parts.append(label("hide Guides + Reference before you export", 39, 97.6, 2.1, "middle"))
    return "\n".join(parts)


def reference_fragment(kind: Kind, slug: str) -> str:
    """Re-emit only the filled geometry of a vendored catalog asset."""
    src = (HERE / f"vendor/{kind.key}s/{slug}.svg").read_text()
    root = ET.fromstring(src)
    out = []
    for el in root.iter():
        tag = el.tag.split("}")[-1]
        if el.get("fill") == "none":
            continue  # e.g. the no-op <circle fill="none"> in heads/default.svg
        if tag == "path":
            out.append(f'<path d="{el.get("d")}"/>')
        elif tag == "circle":
            out.append(f'<circle cx="{el.get("cx")}" cy="{el.get("cy")}" r="{el.get("r")}"/>')
        elif tag in {"polygon", "rect", "ellipse"}:
            attrs = " ".join(f'{k}="{v}"' for k, v in el.attrib.items()
                             if k in {"points", "x", "y", "width", "height", "cx", "cy", "rx", "ry"})
            out.append(f"<{tag} {attrs}/>")
    return f'<g fill="{REF_FILL}">' + "".join(out) + "</g>"


def standalone_svg(fragment: str, px: int = CANVAS_PX, background: str | None = None) -> str:
    bg = f'<rect width="100" height="100" fill="{background}"/>' if background else ""
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" '
            f'width="{px}" height="{px}">{bg}{fragment}</svg>')


def rasterise(fragment: str, px: int = CANVAS_PX, background: str | None = None) -> Image.Image:
    png = cairosvg.svg2png(bytestring=standalone_svg(fragment, px, background).encode(),
                           output_width=px, output_height=px)
    return Image.open(io.BytesIO(png)).convert("RGBA")


# --------------------------------------------------------------------------
# SVG template
# --------------------------------------------------------------------------


def build_svg(kind: Kind) -> str:
    refs = []
    for slug in kind.references:
        refs.append(
            f'  <g id="{REF_ID_PREFIX}{slug}" inkscape:groupmode="layer" '
            f'inkscape:label="Reference: {slug} {kind.key} (optional)" '
            f'sodipodi:insensitive="true" style="display:none">\n'
            f'    {reference_fragment(kind, slug)}\n  </g>')
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<!-- Battlesnake {kind.key} template. Draw your {kind.key} in the "draw-here" layer in
     one dark fill colour. Details are holes: anything left empty or filled white
     shows the board through. Strokes and overlapping shapes are fine; the studio
     converts and merges them. Hide the guides and reference layers, export SVG
     (or a 1000x1000 PNG) and upload it to the Battlesnake Head & Tail Studio. -->
<svg xmlns="http://www.w3.org/2000/svg"
     xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape"
     xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd"
     viewBox="0 0 100 100" width="1000" height="1000">
  <sodipodi:namedview id="namedview" pagecolor="#ffffff" inkscape:current-layer="{DRAW_ID}"
     inkscape:document-units="px"/>
{chr(10).join(refs)}
  <g id="{DRAW_ID}" inkscape:groupmode="layer" inkscape:label="Draw here (black)" fill="#000000">
  </g>
  <g id="{GUIDES_ID}" inkscape:groupmode="layer" inkscape:label="Guides (hide before export)"
     sodipodi:insensitive="true" style="mix-blend-mode:multiply">
{guides_fragment(kind)}
  </g>
</svg>
"""


# --------------------------------------------------------------------------
# PSD template
# --------------------------------------------------------------------------
def lock_all(layer) -> None:
    """Photoshop "Lock All" (lspf = 0x80000000). Procreate's PSD import keeps locks.

    psd-tools 1.23 Layer.lock() is a no-op on a layer that has no 'lspf' block
    yet (it mutates a detached ProtectedSetting), so write the block directly.
    """
    layer.tagged_blocks.set_data(Tag.PROTECTED_SETTING, int(ProtectedFlags.COMPLETE))


def build_psd(kind: Kind, path: Path, drawing: Image.Image | None = None) -> None:
    """Write the layered template. `drawing` (RGBA) is for tests only."""
    psd = PSDImage.new("RGB", (CANVAS_PX, CANVAS_PX), color=(255, 255, 255))
    # create_pixel_layer adds at the top, so build bottom -> top.
    bg = psd.create_pixel_layer(Image.new("RGBA", (CANVAS_PX, CANVAS_PX), (255, 255, 255, 255)),
                                name="Background (leave on)")
    lock_all(bg)
    for slug in reversed(kind.references):
        ref = psd.create_pixel_layer(rasterise(reference_fragment(kind, slug)),
                                     name=f"Reference: {slug} {kind.key} (optional)")
        ref.visible = False
        lock_all(ref)
    psd.create_pixel_layer(drawing or Image.new("RGBA", (CANVAS_PX, CANVAS_PX), (0, 0, 0, 0)),
                           name="Draw here (black)")
    # Multiply: guides show over white but vanish over black ink, so an export
    # made with the guides still on keeps every ink pixel intact (the studio
    # can then just ignore the light, saturated guide pixels).
    guides = psd.create_pixel_layer(rasterise(guides_fragment(kind)),
                                    name="Guides (hide before export)",
                                    blend_mode=BlendMode.MULTIPLY)
    lock_all(guides)
    # The merged composite (what non-layer-aware readers and thumbnails show)
    # is written RAW by default: 3 MB. RLE brings the whole file to ~0.6 MB.
    psd._record.image_data.compression = Compression.RLE
    psd.save(path)


def verify_psd(path: Path) -> list[str]:
    psd = PSDImage.open(path)
    rows = [f"{path.name}: {psd.width}x{psd.height} mode={psd.color_mode.name} depth={psd.depth}"]
    for layer in reversed(list(psd)):  # top -> bottom
        rows.append(f"  - {layer.name!r:44} visible={layer.visible!s:5} "
                    f"opacity={layer.opacity:3} blend={layer.blend_mode.name:8} locks={hex(layer.locks.value) if layer.locks is not None else None} "
                    f"bbox={layer.bbox}")
    return rows


SVG_NS = "{http://www.w3.org/2000/svg}"
COLOUR_RE = re.compile(r"#[0-9a-fA-F]{6}")


def check_contract(kind: Kind, svg: str) -> None:
    """Fail loudly if the SVG template breaks the studio's template contract.

    - ids: exactly one `draw-here`, one `guides`, and one `reference-<slug>` per
      reference, all top-level layers;
    - colours: every fill/stroke under `guides` and `reference-*` is in PALETTE.
    """
    root = ET.fromstring(svg.split("?>", 1)[1])
    layers = {g.get("id"): g for g in root.findall(f"{SVG_NS}g")}
    want = {DRAW_ID, GUIDES_ID} | {f"{REF_ID_PREFIX}{s}" for s in kind.references}
    if set(layers) != want:
        raise SystemExit(f"{kind.key}: layer ids {sorted(layers)} != {sorted(want)}")
    palette = {c.lower() for c in PALETTE}
    for lid, g in layers.items():
        if lid == DRAW_ID:
            continue
        for el in g.iter():
            for attr in ("fill", "stroke"):
                v = el.get(attr)
                if v is None or v == "none":
                    continue
                if v.lower() not in palette:
                    raise SystemExit(f"{kind.key}: {lid} uses non-palette {attr}={v}")
            for c in COLOUR_RE.findall(el.get("style") or ""):
                if c.lower() not in palette:
                    raise SystemExit(f"{kind.key}: {lid} uses non-palette style colour {c}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT,
                    help=f"output directory (default: {DEFAULT_OUT})")
    ap.add_argument("--previews", action="store_true", help="also render previews/")
    args = ap.parse_args()
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)
    for kind in (HEAD, TAIL):
        stem = f"{OUTPUT_PREFIX}-{kind.key}"
        svg = build_svg(kind)
        check_contract(kind, svg)
        (out / f"{stem}-template.svg").write_text(svg)
        build_psd(kind, out / f"{stem}-template.psd")
        guide = rasterise(guides_fragment(kind))
        guide.save(out / f"{stem}-guide.png", optimize=True)
        print("\n".join(verify_psd(out / f"{stem}-template.psd")))
        if args.previews:
            PREVIEWS.mkdir(exist_ok=True)
            write_previews(kind, out / f"{stem}-template.psd")
    example = example_drawing.render()
    example.save(out / "example-drawing.png", optimize=True)
    print("\n".join(example_drawing.describe(example)))
    print(f"palette: {' '.join(PALETTE)}")
    print(f"wrote {out}")
    return 0


def write_previews(kind: Kind, psd_path: Path) -> None:
    psd = PSDImage.open(psd_path)
    psd.composite(force=True).save(PREVIEWS / f"{kind.key}-psd-composite.png")
    # Same file with references turned on (what the artist sees after toggling).
    first_ref = f"Reference: {kind.references[0]} {kind.key} (optional)"
    for layer in psd:
        if layer.name == first_ref:
            layer.visible = True
    psd.composite(force=True).save(PREVIEWS / f"{kind.key}-psd-composite-refs.png")


if __name__ == "__main__":
    sys.exit(main())
