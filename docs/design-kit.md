# Head & Tail design kit

`server/src/design_kit` (exposed as `arena::design_kit`) turns an untrusted upload of a
Battlesnake head or tail into **one clean path** in a `0 0 100 100` viewBox, plus shape
metrics and friendly lints. It is pure and `AppState`-free: no I/O, no database, no async.
The Head & Tail Studio (a later PR) calls it from an endpoint; tests call it directly.

Status (DEV-1539):

| PR | Adds |
|---|---|
| 1 (this) | core, PNG/JPEG input, the ink rule, lints, fixes |
| 2 | SVG input and hardening, the full catalog corpus test, reference shapes |
| 3 | board component, studio page, endpoint and its guards |
| 4 | templates, guide page, discoverability |

Until PR 2, SVG uploads are recognised and rejected with `not_yet_supported`.

## Asset contract

From the BattlesnakeOfficial/board source and a census of all 184 catalog files:

- Every asset has `viewBox="0 0 100 100"`.
- The board fetches `media.battlesnake.com/snakes/{heads,tails}/<slug>.svg`, takes
  `template.content.firstChild.innerHTML` and injects it into
  `<svg viewBox="0 0 100 100" fill={color} …><g transform=T>`. A leading `<?xml?>` or
  comment breaks this, so **our output always starts with `<svg`**. Explicit fills
  override the snake colour; `<style>` and ids leak page-wide. That is why we never echo
  user markup: the output is a fixed template with a numeric path.
- **Heads** face right; the neck covers the full left edge (x = 0, y 0–100). The board
  mirrors or rotates them for the other directions.
- **Tails** join the body on the left edge and point right.
- Content outside the square is clipped by the board's nested `<svg>`.

Output is exactly:

```
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="evenodd" d="…"/></svg>
```

where `d` uses only absolute `M L Q C Z`, digits, `.`, `-` and spaces (2 decimals).

## Pipeline (raster)

`process_upload(bytes, &Limits, &[Fix]) -> Result<CleanShape, ProcessError>`:

1. **Sniff** magic bytes. PNG and JPEG are processed; SVG is recognised. HEIC/AVIF
   (`ftyp` with an image brand), GIF, WebP, PSD (`8BPS`), ZIP/.procreate (`PK\x03\x04`)
   and PDF/.ai (`%PDF`, `%!PS`) are rejected with app-specific advice
   (`unsupported_format`).
2. **Byte cap** for the sniffed format (`too_large`).
3. **Decode** with the header dimensions checked *before* any pixel buffer is allocated
   (`image_too_large`; a 100000² PNG header is rejected in microseconds).
   - PNG (`png`): palette, low bit depth, tRNS and 16-bit are normalised to 8-bit;
     only the first APNG frame is used. ICC profiles and text chunks are skipped, not
     inflated, and the decoder's own allocations are capped at 4 MiB.
   - JPEG (`zune-jpeg`, strict mode, so a truncated file is `invalid_image` rather than
     grey filler rows): decoded to RGB. Progressive JPEGs keep every DCT coefficient
     until the last scan, so they are capped at `max_raster_side`² × 3 / (2 ×
     components) pixels (1448 px square for colour, 2048 for greyscale).
   - EXIF orientation (JPEG) is applied after the ink rule, so a camera photo that the
     browser shows upright is traced upright.
4. **Ink rule** (below) turns pixels into ink coverage 0–255.
5. **Square grid**: the canvas is fitted uniformly and centred into a square grid of
   `clamp(max(w, h), 512, 1024)` px (box downscale, bilinear upscale), thresholded at
   50%. A non-square canvas gets the `non_square` tip.
6. **Specks and pinholes** smaller than 1 unit² (10 × 10 px on the 1000 px template) are
   removed (`specks_removed`).
7. **Budget** the tracer's work before it runs (`too_complex` otherwise; see below).
8. **Trace** with visioncortex (the engine behind vtracer's binary mode) inside
   `catch_unwind`; degenerate splines are dropped. Holes come out with opposite
   winding, emitted as evenodd.
9. **Fixes** (optional, see below), **emit** `d` (over 64 KiB is `too_complex`),
   **measure** on a 200 px mask and **lint** for both kinds.

`process_upload` is synchronous and CPU-bound. Run it off the async runtime, behind a
semaphore.

### Limits (`Limits::default()`)

| Limit | Value |
|---|---|
| SVG bytes | 512 KiB (applied after sniffing; SVG processing arrives in PR 2) |
| raster bytes | 4 MiB |
| raster side | 2048 px (from the header) |
| min useful side | 128 px (smaller gets the `low_resolution` tip) |
| progressive JPEG | `max_raster_side`² × 3 / (2 × components) pixels |
| trace grid | 1024 px |
| traced clusters | 2000 |
| outline on the trace grid | 64 boundary edges per px of grid side (65,536 at 1024) |
| shapes' and holes' bounding boxes | 8 × the grid's area |
| output `d` | 64 KiB |

### The trace budget

visioncortex's cost is not linear in the image. Each cluster's whole bounding box is
rescanned, as are its holes' boxes, and path simplification is quadratic along perfectly
straight runs such as 45° staircases. It also panics past internal limits: a `u16`
cluster index (65,535 provisional clusters per scan) and a 1,000,000-step outline walk.
Before the budget, a 3.5 KB PNG of 2 px diagonal stripes took 2.3 s of CPU in release and
came back `ok`, and a comb or a 1 px serpentine panicked.

So the cleaned mask is checked before tracing:

- **Outline**: boundary edges (ink pixel sides facing background or the grid border) ≤
  64 × side. Every provisional cluster in a scan starts at a pixel whose top and left
  sides are boundary edges, and every outline walk follows one boundary, so this also
  keeps the scans under 65,535 clusters and the walks under 1,000,000 steps (the budget
  is clamped to guarantee it). The most detailed official asset needs 20 × side (21
  when roughened like a hand drawing).
- **Boxes**: the bounding boxes of the shapes and enclosed holes cover ≤ 8 × the grid
  (the official assets need at most 2.2; five concentric rings need about 7.6).
- **Shapes**: at most 2000.

### Cost (release, measured on the dev VM, median of 15 runs)

| Input | Time | Peak RSS (whole process) |
|---|---|---|
| 512 px PNG | 12 ms | |
| 1024 px PNG | 45 ms | 16 MB |
| 2048 px PNG | 66–72 ms | 23 MB |
| 2048 px JPEG q80 | 62 ms | 19 MB |
| 1448 px progressive JPEG (the cap) | | 21 MB |
| 2048 px PNG with a 57 KB iCCP chunk that inflates to 40 MB | | 23 MB |
| 1024 px random noise | 57 ms → `too_complex` | |
| 1024 px checkerboards (12 / 24 px squares) | 22 / 23 ms → `too_complex` | |
| crafted stripes, rings, comb, serpentine (over the budget) | 23–40 ms → `too_complex` | |
| worst crafted input found under the budget (2 px diagonal lines, rings) | about 130 ms | |

A debug build takes about 0.09 s (512), 0.3 s (1024) and 0.6 s (2048) for the same
PNGs. The image crates are built with `opt-level = 3` even in dev (root `Cargo.toml`),
which keeps the debug test suite and the debug e2e server usable.

## The ink rule and the template palette

The templates draw guides in five saturated colours and the optional reference shapes in
one light ghost colour (`design_kit::palette`; the template generator must match):

| Role | Colour |
|---|---|
| grid | `#bfe3f7` |
| border, centre line, small labels | `#2f8fd6` |
| labels, direction arrow | `#1f6fb0` |
| attach edge band and ticks | `#ff4f86` |
| attach edge label | `#d42a63` |
| reference ghost | `#c8c2d4` |

The guides layer uses Multiply, so guides over a visible reference come out as the
product of the two colours. A pixel is **never ink** when it is within RGB distance 48 of:

- a palette colour, or the segment from it to white (anti-aliased edges over the white
  background);
- when the reference ghost is visible (at least 0.1% of the pixels are solid and within
  24 of it), also the product of each guide colour and the ghost, or the segment from it
  to the ghost. The products are dark navy and crimson (luma 64–76), so without a
  visible ghost they are ordinary ink: a navy or crimson drawing still works.

Then:

- **Alpha raster** (at least 0.5% of pixels transparent and 0.1% opaque): coverage is
  the pixel's alpha. Near-white pixels are not ink either, so white details painted on
  the shape (eyes, stripes) become holes, as they would in an opaque export.
- **Opaque raster** (everything else, including every JPEG): composite over white;
  coverage is the darkness `255 − luma` (Rec. 709), so after the 50% threshold ink is
  luma < 0.5.

Info lints from the same pass:

- `guides_visible`: at least 0.1% of pixels are within distance 24 of a palette colour
  (or of a product, when the ghost is visible) *and* visibly coloured (chroma ≥ 12; the
  ghost has 18). The chroma check keeps grey anti-aliasing of black ink, pencil and
  paper from counting. "We ignored the template guides. Hide them next time for the
  cleanest result."
- `colours_flattened`: at least 1% of the canvas is ink in a clear colour (chroma ≥ 64),
  or (alpha rasters) at least 0.1% of the canvas was solid white and became holes.
- `semi_transparent`: more than 5% of the visible ink, and at least 0.1% of the canvas,
  is between 10% and 90% opaque (soft brushes, low layer opacity).

Ink is meant to be black. Light colours and dark colours near the template's blues and
pinks are dropped, and the `empty` message says so.

We never recommend a transparent export: both kinds of export work.

Tested by compositing a drawn head and tail with the vendored guide overlays and a
rendered reference ghost in all eight Background × Guides × Reference combinations
(IoU ≥ 0.99 against the clean drawing; the info lints are exactly `[guides_visible]`
when guides or the reference are visible, and empty otherwise). The guide overlays are
copies of the template generator's output (`battlesnake-{head,tail}-guide.png`). Where
its pink attach label meets a blue guide line, a few anti-aliased purple pixels match no
template colour; they sit inside the neck, under any drawing, and on a guides-only
export they are removed as 3 specks.

## Metrics

Measured on one **200 px** mask of the clean path (alpha > 127 is filled), the same
resolution the thresholds were derived at:

- `fill_pct`: share of the square covered.
- `left/right/top/bottom_edge_pct`: share of rows (columns) whose outermost 1-unit
  strip (2 px) is at least half filled.
- `left_edge_gaps`: up to 16 `[y0, y1)` ranges where the left edge is open, so the
  studio can bracket them on the close-up.
- `bbox` and `centroid` (units), `holes` (4-connected background regions that don't
  touch the border) and `hole_pct` (their area as a share of the silhouette with its
  holes filled in).
- `drawing_edges`: the same edge coverage measured along the sides of `bbox`, over the
  rows or columns the drawing spans. For a drawing that reaches every edge it equals the
  square's edges; for a padded or short one it still says which side is full height.

`Metrics::from_alpha` is anchored to an independent oracle: resvg renders of the
vendored catalog samples, measured with it, match the rsvg-derived
`catalog/metrics_summary.csv` for every metric a lint reads: fill% and edge% within 0.5
points (the test allows 2), the centroid within 0.12 units (allows 0.5), the hole
fraction within 0.1 points (allows 1) and the bounds exactly (allows 1)
(`server/tests/design_kit_metrics.rs`).

## Lints

Shape lints depend on the kind, so `CleanShape.lints` has both `head` and `tail` lists
and the client can switch kinds without re-posting. Kind-independent input facts are in
`CleanShape.info`. Each lint has `code()`, `severity()` (warn, tip, info), `message()`,
`fix()` and `guide_anchor()`.

Catalog statistics (rsvg, 200 px; heads n = 101, tails n = 83; min / p5 / median / p95
/ max):

| Metric | Heads | Tails |
|---|---|---|
| fill | .374 / .487 / .670 / .849 / .879 | .204 / .375 / .538 / .873 / 1.00 |
| left edge | .88 / .97 / 1 / 1 / 1 | .88 / .971 / 1 / 1 / 1 |
| right edge | 0 / 0 / .07 / .385 / .475 | 0 / 0 / .04 / .495 / 1.00 |
| centroid x | 29.4 / 35.8 / 41.9 / 46.4 / 48.5 | 26.6 / 30.1 / 38.6 / 47.9 / 56.7 |
| hole fraction | 0 / 0 / .043 / .252 / .471 | 0 / 0 / 0 / .225 / .429 |

Every asset touches x = 0 and spans y from 0–2 to at least 98.5.

| Code | Rule | Severity | Kinds | Why this threshold |
|---|---|---|---|---|
| `nearly_empty` | fill < 15% | warn | both | catalog min 20.4% (mouse tail) |
| `solid_square` | fill > 95% | warn | head | heads max 87.9%; tails exempt (block-bum is 100%) |
| `neck_gap` | left edge < 85% | warn | both | catalog min 88% (pumpkin head) |
| `margins` | bbox x0 > 2, y0 > 3 or y1 < 97 | warn, offers **Fit** while it would help | both | every asset touches x = 0 and spans 0–2 … ≥ 98.5 |
| `faces_left` | the drawing's full-height side is the right one; or no side is full height and centroid x > 52 | warn, offers **Flip** | head | heads' right edge max 47.5%, centroid x max 48.5 |
| `faces_up_down` | the drawing's full-height side is the top or bottom; or its left side is full height and its right side > 60% (not a solid square) | warn | head | heads' left edge never < 88%, right edge max 47.5% |
| `tail_reversed` | the drawing's left side < 85% and its right, top or bottom ≥ 85% | warn, offers **Flip** when it's the right side | tail | tails' left edge never < 88% |
| `outline_only` | hole fraction > 55% | warn | both | catalog max 47.1%; a 6-unit stroke outline is ≈ 77% |
| `specks_removed` | pieces or holes < 1 unit² removed | info | – | removed automatically |
| `colours_flattened` | see the ink rule | info | – | |
| `guides_visible` | see the ink rule | info | – | |
| `semi_transparent` | see the ink rule | info | – | |
| `non_square` | canvas not square | tip | – | |
| `low_resolution` | longer side < 128 px | tip | – | |

Direction is judged by where the drawing's full-height side is (`drawing_edges`), not
by the square's edges or the centre of mass alone. A short or padded drawing whose
left side is full height isn't mistaken for a rotated one, a quarter turn of a
top-heavy head (beluga's centre of mass moves to x 53.5) isn't mistaken for a mirror, and
a mirrored head whose mass is near the middle (guitar, x 51.5) still gets Flip. The
centroid rule only applies when no side is full height.

Fit keeps the aspect ratio: it scales the larger side to 100, left-anchors the drawing
and centres it vertically. So Fit is offered only while it would clear the margins or
at least move the drawing to the neck edge or make it noticeably bigger; a fitted wide
drawing gets the `margins` warning without Fit, asking for a taller drawing ("Draw it
taller, from edge to edge").

So that each problem gets one message and one fix:

- `margins` suppresses `neck_gap` when it offers Fit and the fitted drawing would have
  a full-height left edge (the drawing's own left side × height / larger side ≥ 85%).
  A small dot or a wide, short drawing keeps both.
- A direction lint suppresses `neck_gap` when it found the full-height side elsewhere.
- `solid_square` suppresses `faces_up_down` (a solid square has a full right side too).

When a kind has no warn-level lint, `CleanShape::passes(kind)` is true: "Passes every
check the official heads pass."

Deferred: thin line art (L5b) and cutout thickness / disappearance at 24 px (L7a, L7b,
L8). The studio's game-size and close-up views show detail loss visually.

## Fixes

`Fix::Flip` and `Fix::Fit` are affine rewrites of the clean path before it is emitted,
then the shape is re-measured and re-linted. `process_upload` takes a set of fixes and
applies Flip before Fit, whatever the order (fitting first would move a flipped head
away from the neck edge). The server keeps nothing between requests, so the studio sends
every fix tapped so far (e.g. `?fix=flip,fit`) with the original upload:

- **Flip**: `x → 100 − x`.
- **Fit**: scale uniformly so the visible bounding box (clipped to the square) fills
  0–100 on its larger side, keeping the aspect ratio; left-anchored (x0 → 0) and
  centred vertically.

## Errors

`ProcessError` (`thiserror`) has `code()`, `user_message()` and `is_internal()`.
Everything except `internal` is caused by the upload and should be shown as a 422.
`internal` (a caught tracer panic, a failed allocation) is a bug: report it as a 500.
Note that a caught panic still runs the panic hook, so it reaches Sentry. The trace
budget keeps crafted uploads clear of the panics we know about (see above).

| Code | When |
|---|---|
| `empty_file` | zero bytes |
| `unknown_format` | not PNG, JPEG or SVG, and not a format we recognise |
| `unsupported_format` | HEIC, GIF, WebP, PSD, ZIP/.procreate, PDF/.ai |
| `not_yet_supported` | SVG, until PR 2 |
| `too_large` | over the byte cap for the format |
| `image_too_large` | wider or taller than 2048 px |
| `invalid_image` | truncated, corrupt or zero-size |
| `too_complex` | over the trace budget, or `d` over 64 KiB (noise, checkerboards, photos, crafted stripes) |
| `empty` | nothing drawable (carries info lints, e.g. guides only) |
| `internal` | a bug |

## Tests

- `server/tests/design_kit_raster.rs`: sniffing; round trips of vendored catalog heads
  and tails rendered with resvg at 512/1024/2048, transparent and opaque, roughened
  (wobble, blur, grain, specks) and JPEG q80 (IoU ≥ 0.97 against the original, left edge
  ≥ 95%, no warnings); navy and crimson drawings; EXIF orientation; the progressive
  JPEG cap; skipped PNG metadata; the ink matrix; one case per lint with exact codes for
  each kind, including a rotated top-heavy head, a mirrored centred head, short and wide
  drawings before and after Fit, and combined fixes; suppression and pass state; fixes
  (including the inverse transform); hostile rasters with exact errors (including
  truncated JPEGs and crafted stripes, rings, a comb and a serpentine stopped by the
  trace budget); and a property test that every output is one clean path.
- `server/tests/design_kit_metrics.rs`: the metric oracle described above.
- Fixtures live in `server/tests/fixtures/design_kit/`: catalog samples and the matching
  rows of `metrics_summary.csv` under `catalog/` (plus `heads/guitar.svg` for the
  direction lints), the template guide overlays under `template/`. Reference-ghost
  overlays are rendered in the tests.

```
cargo test -p arena --test design_kit_raster --test design_kit_metrics
cargo test -p arena --lib design_kit
```
