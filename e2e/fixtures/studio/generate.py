#!/usr/bin/env python3
"""Regenerate the Head & Tail Studio e2e fixtures in this directory.

The outputs are committed; CI never runs this. It needs `rsvg-convert` (librsvg) and
Pillow. Every output is derived from a vendored catalog SVG or drawn from fixed
numbers, so rerunning it gives the same files (byte-identical for a given librsvg
and Pillow version).

    python3 e2e/fixtures/studio/generate.py

Outputs (all at most 512 px):
- head.png          chomp (catalog head), 256 px, black on transparent
- head.jpg          the same on white, as a JPEG
- head.svg          caffeine (catalog head), copied as-is
- tail.svg          alligator (catalog tail), copied as-is
- mirrored-head.png head.png flipped left to right: a head facing left (Flip fixes it)
- round-blob.png    a filled circle: no full-height neck on the left (neck warning)
- template.psd      the first bytes of a Photoshop file (unsupported format)
"""

import io
import shutil
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
CATALOG = HERE.parents[2] / "server" / "tests" / "fixtures" / "design_kit" / "catalog"
SIDE = 256


def render(svg: Path, side: int) -> Image.Image:
    """Rasterize an SVG to an RGBA image with librsvg."""
    png = subprocess.run(
        ["rsvg-convert", "--width", str(side), "--height", str(side), str(svg)],
        check=True,
        capture_output=True,
    ).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def save_png(image: Image.Image, name: str) -> None:
    # No metadata chunks (Pillow adds none by default), fixed compression.
    image.save(HERE / name, format="PNG", optimize=False, compress_level=9)


def on_white(image: Image.Image) -> Image.Image:
    white = Image.new("RGBA", image.size, (255, 255, 255, 255))
    return Image.alpha_composite(white, image).convert("RGB")


def main() -> None:
    head = render(CATALOG / "heads" / "chomp.svg", SIDE)
    save_png(head, "head.png")
    on_white(head).save(HERE / "head.jpg", format="JPEG", quality=90, optimize=False)
    save_png(head.transpose(Image.Transpose.FLIP_LEFT_RIGHT), "mirrored-head.png")

    shutil.copyfile(CATALOG / "heads" / "caffeine.svg", HERE / "head.svg")
    shutil.copyfile(CATALOG / "tails" / "alligator.svg", HERE / "tail.svg")

    blob = Image.new("RGBA", (SIDE, SIDE), (0, 0, 0, 0))
    inset = SIDE // 32
    ImageDraw.Draw(blob).ellipse(
        (inset, inset, SIDE - 1 - inset, SIDE - 1 - inset), fill=(0, 0, 0, 255)
    )
    save_png(blob, "round-blob.png")

    # A PSD file header (signature, version 1, reserved, 1 channel, 1x1, 8-bit, RGB):
    # enough for the format sniff, which is all the studio looks at.
    psd = b"8BPS" + bytes([0, 1]) + bytes(6) + bytes([0, 1])
    psd += (1).to_bytes(4, "big") * 2 + bytes([0, 8, 0, 3])
    (HERE / "template.psd").write_bytes(psd + bytes(64))


if __name__ == "__main__":
    main()
