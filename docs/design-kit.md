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

`process_upload(bytes, &Limits, Option<Fix>) -> Result<CleanShape, ProcessError>`:

1. **Sniff** magic bytes. PNG and JPEG are processed; SVG is recognised. HEIC/AVIF
   (`ftyp` with an image brand), GIF, WebP, PSD (`8BPS`), ZIP/.procreate (`PK\x03\x04`)
   and PDF/.ai (`%PDF`, `%!PS`) are rejected with app-specific advice
   (`unsupported_format`).
2. **Byte cap** for the sniffed format (`too_large`).
3. **Decode** with the header dimensions checked *before* any pixel buffer is allocated
   (`image_too_large`; a 100000² PNG header is rejected in microseconds).
   - PNG (`png`): palette, low bit depth, tRNS and 16-bit are normalised to 8-bit;
     only the first APNG frame is used.
   - JPEG (`zune-jpeg`): decoded to RGB. EXIF orientation is not applied.
4. **Ink rule** (below) turns pixels into ink coverage 0–255.
5. **Square grid**: the canvas is fitted uniformly and centred into a square grid of
   `clamp(max(w, h), 512, 1024)` px (box downscale, bilinear upscale), thresholded at
   50%. A non-square canvas gets the `non_square` tip.
6. **Specks and pinholes** smaller than 1 unit² (10 × 10 px on the 1000 px template) are
   removed (`specks_removed`).
7. **Trace** with visioncortex (the engine behind vtracer's binary mode) inside
   `catch_unwind`; degenerate splines are dropped. Holes come out with opposite
   winding, emitted as evenodd. More than 2000 separate shapes is `too_complex`.
8. **Fix** (optional, see below), **emit** `d` (over 64 KiB is `too_complex`),
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
| trace grid | 1024 px |
| traced clusters | 2000 |
| output `d` | 64 KiB |

### Cost (release, measured on the dev VM, median of 15 runs)

| Input | Time | Peak RSS (whole process) |
|---|---|---|
| 512 px PNG | 11 ms | |
| 1024 px PNG | 42 ms | 16 MB |
| 2048 px PNG | 62 ms | 23 MB |
| 2048 px JPEG q80 | 56 ms | 19 MB |
| 1024 px random noise | 94 ms → `too_complex` | |
| 1024 px checkerboards (12 / 24 px squares) | 32 / 40 ms → `too_complex` | |

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

- a palette colour, or the product of a guide colour and the ghost, or
- an anti-aliased blend of one: the segment from each palette colour to white (label
  halos, the white background) and from each product to the ghost.

Then:

- **Alpha raster** (at least 0.5% of pixels transparent and 0.1% opaque): coverage is
  the pixel's alpha. Near-white pixels are not ink either: white details become holes
  and the white halos behind template labels disappear.
- **Opaque raster** (everything else, including every JPEG): composite over white;
  coverage is the darkness `255 − luma` (Rec. 709), so after the 50% threshold ink is
  luma < 0.5.

Info lints from the same pass:

- `guides_visible`: at least 0.1% of pixels are within distance 24 of a palette colour
  (or product) *and* visibly coloured (chroma ≥ 12; the ghost has 18). The chroma
  check keeps grey anti-aliasing of black ink, pencil and paper from counting. "We
  ignored the template guides. Hide them next time for the cleanest result."
- `colours_flattened`: at least 1% of pixels are ink in a clear colour (chroma ≥ 64), or
  (alpha rasters, guides not visible) at least 0.1% of solid pixels were white and
  became holes.
- `semi_transparent`: more than 5% of the visible ink is between 10% and 90% opaque
  (soft brushes, low layer opacity).

We never recommend a transparent export: both kinds of export work.

Tested by compositing a drawn head and tail with the vendored guide overlays and a
rendered reference ghost in all eight Background × Guides × Reference combinations
(IoU ≥ 0.99 against the clean drawing; the info lints are exactly `[guides_visible]`
when guides or the reference are visible, and empty otherwise).

## Metrics

Measured on one **200 px** mask of the clean path (alpha > 127 is filled), the same
resolution the thresholds were derived at:

- `fill_pct`: share of the square covered.
- `left/right/top/bottom_edge_pct`: share of rows (columns) whose outermost 1-unit
  strip (2 px) is at least half filled.
- `left_edge_gaps`: up to 16 `[y0, y1)` ranges where the left edge is open, so the
  studio can bracket them on the close-up.
- `bbox`, `centroid` (units), `holes`, `pieces`, `hole_pct` (enclosed holes as a share
  of the silhouette with its holes filled in), and `fit_left_edge_pct` (the left-edge
  coverage the shape would have after Fit).

`Metrics::from_alpha` is anchored to an independent oracle: resvg renders of the
vendored catalog samples, measured with it, match the rsvg-derived
`catalog/metrics_summary.csv` within 0.5 points for fill% and every edge%
(`server/tests/design_kit_metrics.rs`; the test allows 2).

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
| `margins` | bbox x0 > 2, y0 > 3 or y1 < 97 | warn, offers **Fit** | both | every asset touches x = 0 and spans 0–2 … ≥ 98.5 |
| `faces_left` | centroid x > 52 | warn, offers **Flip** | head | heads max 48.5; a mirrored head is ≈ 57 |
| `faces_up_down` | right edge > 60%, or left edge < 85% with top/bottom ≥ 85% | warn | head | heads' right edge max 47.5%; left edge never < 88% |
| `tail_reversed` | left edge < 85% and max(right, top, bottom) ≥ 85% | warn, offers **Flip** when it's the right edge | tail | tails' left edge never < 88% |
| `outline_only` | hole fraction > 55% | warn | both | catalog max 47.1%; a 6-unit stroke outline is ≈ 77% |
| `specks_removed` | pieces or holes < 1 unit² removed | info | – | removed automatically |
| `colours_flattened` | see the ink rule | info | – | |
| `guides_visible` | see the ink rule | info | – | |
| `semi_transparent` | see the ink rule | info | – | |
| `non_square` | canvas not square | tip | – | |
| `low_resolution` | longer side < 128 px | tip | – | |

So that each problem gets one message and one fix:

- `margins` suppresses `neck_gap` when Fit would close the gap (`fit_left_edge_pct` ≥
  85%). A small dot keeps both, because Fit wouldn't give it a neck.
- A direction lint (`faces_left`, `faces_up_down`, `tail_reversed`) suppresses
  `neck_gap` when the full-height edge is on another side.
- `faces_left` and `solid_square` suppress `faces_up_down` (a mirrored head and a
  solid square also have a full right edge).

When a kind has no warn-level lint, `CleanShape::passes(kind)` is true: "Passes every
check the official heads pass."

Deferred: thin line art (L5b) and cutout thickness / disappearance at 24 px (L7a, L7b,
L8). The studio's game-size and close-up views show detail loss visually.

## Fixes

`Fix::Flip` and `Fix::Fit` (`?fix=flip|fit` on the studio endpoint) are affine rewrites
of the clean path before it is emitted, then the shape is re-measured and re-linted:

- **Flip**: `x → 100 − x`.
- **Fit**: scale uniformly so the visible bounding box (clipped to the square) fills
  0–100 on its larger side, keeping the aspect ratio; left-anchored (x0 → 0) and
  centred vertically.

## Errors

`ProcessError` (`thiserror`) has `code()`, `user_message()` and `is_internal()`.
Everything except `internal` is caused by the upload and should be shown as a 422.
`internal` (a caught tracer panic, a failed allocation) is a bug: report it as a 500.
Note that a caught panic still runs the panic hook, so it reaches Sentry.

| Code | When |
|---|---|
| `empty_file` | zero bytes |
| `unknown_format` | not PNG, JPEG or SVG, and not a format we recognise |
| `unsupported_format` | HEIC, GIF, WebP, PSD, ZIP/.procreate, PDF/.ai |
| `not_yet_supported` | SVG, until PR 2 |
| `too_large` | over the byte cap for the format |
| `image_too_large` | wider or taller than 2048 px |
| `invalid_image` | truncated, corrupt or zero-size |
| `too_complex` | over 2000 shapes, or `d` over 64 KiB (noise, checkerboards, photos) |
| `empty` | nothing drawable (carries info lints, e.g. guides only) |
| `internal` | a bug |

## Tests

- `server/tests/design_kit_raster.rs`: sniffing; round trips of vendored catalog heads
  and tails rendered with resvg at 512/1024/2048, transparent and opaque, roughened
  (wobble, blur, grain, specks) and JPEG q80 (IoU ≥ 0.97 against the original, left edge
  ≥ 95%, no warnings); the ink matrix; one case per lint with exact codes for each kind;
  suppression and pass state; fixes (including the inverse transform); hostile rasters
  with exact errors; and a property test that every output is one clean path.
- `server/tests/design_kit_metrics.rs`: the metric oracle described above.
- Fixtures live in `server/tests/fixtures/design_kit/`: catalog samples and the matching
  rows of `metrics_summary.csv` under `catalog/`, the template guide overlays under
  `template/`. Reference-ghost overlays are rendered in the tests.

```
cargo test -p arena --test design_kit_raster --test design_kit_metrics
cargo test -p arena --lib design_kit
```
