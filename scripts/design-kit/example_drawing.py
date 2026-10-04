"""The "Try an example" drawing: a friendly hand-drawn-looking snake head.

An original design (not a catalog head), drawn the way the guide tells an artist
to work in Procreate: silhouette inked with the Studio Pen and filled with
ColorDrop, details erased with the same pen. We imitate that with smooth curves
whose edges wobble slightly (low-frequency, seeded, so the output is
deterministic) and anti-aliased edges from 4x supersampling.

Coordinates are template pixels: 1000x1000, y down, head faces right, the neck
fills the whole left edge.
"""

from __future__ import annotations

import math

import numpy as np
from PIL import Image, ImageDraw

SIZE = 1000
SS = 4  # supersampling factor
SEED = 1539

INK = 0
PAPER = 255

Point = tuple[float, float]


# --------------------------------------------------------------------------
# Geometry helpers
# --------------------------------------------------------------------------
def cubic(p0: Point, p1: Point, p2: Point, p3: Point, n: int) -> list[Point]:
    pts = []
    for i in range(n):
        t = i / n
        u = 1 - t
        pts.append((u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0],
                    u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1]))
    return pts


def path(start: Point, segments: list[tuple[Point, Point, Point]], n: int = 80) -> list[Point]:
    """Open polyline from a start point and cubic segments (c1, c2, end)."""
    pts: list[Point] = []
    cur = start
    for c1, c2, end in segments:
        pts += cubic(cur, c1, c2, end, n)
        cur = end
    pts.append(cur)
    return pts


class Wobble:
    """Seeded, smooth 1-D noise: a hand that isn't quite steady."""

    def __init__(self, rng: np.random.Generator, amp: float, periods: tuple[float, ...]):
        self.terms = [(amp * rng.uniform(0.5, 1.0) / (i + 1) ** 0.5,
                       2 * math.pi / p, rng.uniform(0, 2 * math.pi))
                      for i, p in enumerate(periods)]

    def __call__(self, s: float) -> float:
        return sum(a * math.sin(w * s + ph) for a, w, ph in self.terms)


def wobble_polyline(pts: list[Point], wob: Wobble) -> list[Point]:
    """Displace each point along the polyline normal by wob(arc length)."""
    out = []
    s = 0.0
    for i, (x, y) in enumerate(pts):
        if i:
            s += math.dist(pts[i - 1], pts[i])
        a = pts[max(i - 1, 0)]
        b = pts[min(i + 1, len(pts) - 1)]
        dx, dy = b[0] - a[0], b[1] - a[1]
        ln = math.hypot(dx, dy) or 1.0
        nx, ny = -dy / ln, dx / ln
        d = wob(s)
        out.append((x + nx * d, y + ny * d))
    return out


def blob(cx: float, cy: float, r: float, rng: np.random.Generator,
         squash: float = 1.0, tilt: float = 0.0, irregular: float = 0.03) -> list[Point]:
    """A hand-erased round dot: not quite a circle."""
    harmonics = [(irregular * rng.uniform(0.4, 1.0), k, rng.uniform(0, 2 * math.pi))
                 for k in (2, 3)]
    pts = []
    n = 180
    for i in range(n):
        th = 2 * math.pi * i / n
        rr = r * (1 + sum(a * math.sin(k * th + ph) for a, k, ph in harmonics))
        x, y = rr * math.cos(th), rr * squash * math.sin(th)
        ct, st = math.cos(tilt), math.sin(tilt)
        pts.append((cx + x * ct - y * st, cy + x * st + y * ct))
    return pts


def scaled(pts: list[Point]) -> list[Point]:
    return [(x * SS, y * SS) for x, y in pts]


# --------------------------------------------------------------------------
# The drawing
# --------------------------------------------------------------------------
def shapes() -> list[tuple[int, list[Point]]]:
    """(colour, polygon) in paint order."""
    rng = np.random.default_rng(SEED)
    # Mostly slow drift (a steady-ish hand), a touch of fine tremor.
    edge = Wobble(rng, 3.4, (430.0, 240.0, 130.0))
    tremor = Wobble(rng, 0.45, (37.0, 23.0))

    # Silhouette: the neck runs off the top, left and bottom of the canvas (as a
    # ColorDrop fill would), then a big rounded brow and snout.
    top = path((-40, -40), [
        ((150, -40), (300, -40), (380, -12)),
        ((520, 30), (690, 70), (805, 165)),
        ((905, 250), (968, 330), (968, 425)),
        ((968, 448), (962, 462), (955, 470)),
    ])
    jaw = path((952, 612), [
        ((962, 650), (950, 725), (895, 792)),
        ((820, 885), (680, 950), (520, 985)),
        ((420, 1010), (300, 1040), (-40, 1040)),
    ])
    # Mouth: the gap between the two lips, open at the right, curling up into a
    # smile at the inner end (where a pen-eraser stroke tapers off).
    upper_lip = path((955, 470), [
        ((880, 488), (790, 500), (720, 492)),
        ((688, 489), (664, 474), (648, 446)),
    ])
    lower_lip = path((648, 446), [
        ((672, 510), (712, 542), (778, 556)),
        ((848, 572), (912, 590), (952, 612)),
    ])
    # One continuous pen path, so the wobble has no seams at the lip corners.
    silhouette = wobble_polyline(
        wobble_polyline(top + upper_lip[1:] + lower_lip[1:] + jaw[1:], edge), tremor)

    # A little fang hanging from the upper lip (ink, drawn after the mouth).
    fang = path((834, 488), [
        ((842, 506), (850, 524), (858, 538)),
        ((866, 526), (874, 506), (882, 482)),
    ], n=30)

    # Big round eye, pupil looking forward, with a sparkle.
    # Ring stays >= 40 px all round so it survives the trip to game size.
    eye = blob(340, 300, 128, rng, squash=1.03, tilt=0.3, irregular=0.015)
    pupil = blob(358, 290, 66, rng, squash=1.05, tilt=-0.2, irregular=0.015)
    sparkle = blob(376, 266, 22, rng, irregular=0.03)

    # Freckles on the cheek, behind the smile.
    freckles = [blob(552, 600, 25, rng, irregular=0.03),
                blob(612, 632, 23, rng, irregular=0.03),
                blob(542, 668, 22, rng, irregular=0.03)]
    # Nostril near the top of the snout.
    nostril = blob(862, 318, 24, rng, squash=0.8, tilt=-0.5, irregular=0.05)

    out = [(INK, silhouette), (INK, fang),
           (PAPER, eye), (INK, pupil), (PAPER, sparkle), (PAPER, nostril)]
    out += [(PAPER, f) for f in freckles]
    return out


def render() -> Image.Image:
    """1000x1000 8-bit grayscale PNG-ready image: black ink on white."""
    big = Image.new("L", (SIZE * SS, SIZE * SS), PAPER)
    d = ImageDraw.Draw(big)
    for colour, poly in shapes():
        d.polygon(scaled(poly), fill=colour)
    img = big.resize((SIZE, SIZE), Image.Resampling.BOX)
    # Snap near-solid values so the file stays small; keep the soft edge pixels.
    a = np.asarray(img).astype(np.int16)
    a = np.where(a < 8, 0, np.where(a > 247, 255, a)).astype(np.uint8)
    return Image.fromarray(a, "L")


# --------------------------------------------------------------------------
# Self-check against the studio's v1 lint rules (plan decision 8), measured
# the same way: a 200 px mask, ink = darker than mid-grey.
# --------------------------------------------------------------------------
def metrics(img: Image.Image) -> dict[str, float]:
    m = np.asarray(img.resize((200, 200), Image.Resampling.BOX)) < 128
    ys, xs = np.nonzero(m)
    cov = m.mean()
    # Holes = background not connected to the canvas border.
    bg = ~m
    seen = np.zeros_like(bg)
    stack = [(y, x) for y in range(200) for x in (0, 199) if bg[y, x]]
    stack += [(y, x) for x in range(200) for y in (0, 199) if bg[y, x]]
    while stack:
        y, x = stack.pop()
        if seen[y, x] or not bg[y, x]:
            continue
        seen[y, x] = True
        for yy, xx in ((y + 1, x), (y - 1, x), (y, x + 1), (y, x - 1)):
            if 0 <= yy < 200 and 0 <= xx < 200 and not seen[yy, xx]:
                stack.append((yy, xx))
    holes = bg & ~seen
    return {
        "coverage": float(cov),
        "left_edge": float(m[:, 0].mean()),
        "right_edge": float(m[:, -1].mean()),
        "top_edge": float(m[0, :].mean()),
        "bottom_edge": float(m[-1, :].mean()),
        "centroid_x": float(xs.mean() / 2),
        "bbox": (float(xs.min() / 2), float(ys.min() / 2),
                 float((xs.max() + 1) / 2), float((ys.max() + 1) / 2)),
        "hole_fraction": float(holes.sum() / (holes.sum() + m.sum())),
    }


def check(img: Image.Image) -> list[str]:
    """Return the lint codes this drawing would trip as a head (want: none)."""
    k = metrics(img)
    x0, y0, _, y1 = k["bbox"]
    trips = []
    if k["coverage"] < 0.15:
        trips.append("nearly_empty")
    if k["coverage"] > 0.95:
        trips.append("solid_square")
    if k["left_edge"] < 0.85:
        trips.append("neck_gap")
    if x0 > 2 or y0 > 3 or y1 < 97:
        trips.append("margins")
    if k["centroid_x"] > 52:
        trips.append("faces_left")
    if k["right_edge"] > 0.60:
        trips.append("faces_up_down")
    if k["hole_fraction"] > 0.55:
        trips.append("outline_only")
    return trips


def describe(img: Image.Image) -> list[str]:
    k = metrics(img)
    trips = check(img)
    if trips:
        raise SystemExit(f"example-drawing.png trips head lints: {trips} ({k})")
    return [f"example-drawing.png: coverage={k['coverage']:.1%} left_edge={k['left_edge']:.0%} "
            f"right_edge={k['right_edge']:.0%} centroid_x={k['centroid_x']:.1f} "
            f"holes={k['hole_fraction']:.1%} bbox={k['bbox']} lints=none"]
