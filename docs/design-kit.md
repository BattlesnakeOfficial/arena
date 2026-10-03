# Head & Tail design kit

`server/src/design_kit` (exposed as `arena::design_kit`) turns an untrusted upload of a
Battlesnake head or tail into **one clean path** in a `0 0 100 100` viewBox, plus shape
metrics and friendly lints. It is pure and `AppState`-free: no I/O, no database, no async.
The Head & Tail Studio calls it in a short-lived child process, `arena studio-worker`,
which runs it through `process_on_big_stack` (see [Running it](#running-it-the-stack) and
[The studio](#the-studio)); tests call it directly.

Status (DEV-1539):

| PR | Adds |
|---|---|
| 1 | core, PNG/JPEG input, the ink rule, lints, fixes |
| 2 | SVG input and hardening, the full catalog corpus test, reference shapes |
| 3 (this) | board component, studio page, endpoint, its guards and the worker process |
| 4 | templates, guide page, discoverability |

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

1. **Sniff** magic bytes. PNG and JPEG take this path, SVG [its own](#pipeline-svg). HEIC/AVIF
   (`ftyp` with an image brand), GIF, WebP, PSD (`8BPS`), ZIP/.procreate (`PK\x03\x04`),
   PDF/.ai (`%PDF`, `%!PS`), gzip/.svgz (`1f 8b`) and UTF-16 text (a byte order mark, or
   `<` as UTF-16) are rejected with app-specific advice (`unsupported_format`).
2. **Byte cap** for the sniffed format (`too_large`).
3. **Decode** with the header dimensions checked *before* any pixel buffer is allocated
   (`image_too_large`; a 100000² PNG header is rejected in microseconds).
   - PNG (`png`): palette, low bit depth, tRNS and 16-bit are normalised to 8-bit;
     only the first APNG frame is used. ICC profiles and text chunks are skipped, not
     inflated, and the decoder's own allocations are capped at 4 MiB.
   - JPEG (`zune-jpeg`, strict mode, so a truncated file is `invalid_image` rather than
     grey filler rows): decoded to RGB. zune-jpeg keeps every DCT coefficient until the
     last scan when a JPEG is progressive, or when its first scan doesn't carry every
     component (a baseline file with one scan per component, which encoders write when
     they optimise their Huffman tables). Such files are capped at `max_raster_side`² ×
     3 / (2 × components) pixels (1448 px square for 3 components, 1254 for CMYK, 2048
     for greyscale). The first scan is found by walking the markers the way zune-jpeg
     does; a file where it can't be found gets the cap too.
   - A decoder panic (zune-jpeg 0.5 has some, e.g. on a CMYK JPEG with one scan per
     component and subsampled colour) is caught and reported as `invalid_image`.
   - EXIF orientation (JPEG) is applied after the ink rule, so a photo that the browser
     shows upright is traced upright. (Camera originals are 3000–4000 px, over the size
     limit; this is for photos that were cropped or resized and kept the tag.)
4. **Ink rule** (below) turns pixels into ink coverage 0–255.
5. **Square grid**: the canvas is fitted uniformly and centred into a square grid of
   `clamp(max(w, h), 512, 1024)` px (bilinear upscale; box downscale with each source
   pixel weighted by how much of it the grid pixel covers, so a 1025–2047 px export
   traces like a 1024 px one), thresholded at 50%. A non-square canvas gets the
   `non_square` tip.
6. **Specks and pinholes** smaller than 1 unit² (10 × 10 px on the 1000 px template) are
   removed (`specks_removed`).
7. **Budget** the tracer's work before it runs (`too_complex` otherwise; see below).
8. **Trace** with visioncortex (the engine behind vtracer's binary mode) inside
   `catch_unwind`; degenerate splines are dropped. Corners are turns of 40° or more;
   smooth runs get a new cubic every 15° of turn (with vtracer's 45°, one cubic could
   cut a chord across a curve, at some export sizes only; see `trace.rs`). Holes come
   out with opposite winding, emitted as evenodd.
9. **Fixes** (optional, see below), **emit** `d` (over 64 KiB is `too_complex`),
   **measure** on a 200 px mask and **lint** for both kinds.

`process_upload` is synchronous and CPU-bound. Run it off the async runtime, behind a
semaphore: see [Running it](#running-it-the-stack).

### Limits (`Limits::default()`)

| Limit | Value |
|---|---|
| SVG bytes | 512 KiB (applied after sniffing) |
| raster bytes | 4 MiB |
| raster side | 2048 px (from the header) |
| min useful side | 128 px (smaller gets the `low_resolution` tip) |
| progressive or multi-scan JPEG | `max_raster_side`² × 3 / (2 × components) pixels |
| trace grid | 1024 px |
| traced clusters | 2000 |
| outline on the trace grid | 64 boundary edges per px of grid side (65,536 at 1024) |
| shapes' and holes' bounding boxes | 8 × the grid's area |
| output `d` | 64 KiB |
| SVG nodes / nesting / `<use>` / definitions | 20,000 / 64 / 500 / 64 |
| SVG nesting along reference chains | 4,096 levels |
| SVG expansion by references | 20,000 elements |
| SVG CSS work | 20,000,000 steps |
| SVG painted segments / outline travel | 5,000 / 40,000 units |
| SVG dashes / clipped groups | 20,000 / 16 |

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
| 2048 px baseline JPEG, one scan per component (RGB / CMYK), before the cap | 54–86 ms | 39 / 47 MB; now `image_too_large` |
| 2048 px PNG with a 57 KB iCCP chunk that inflates to 40 MB | | 23 MB |
| 1024 px random noise | 57 ms → `too_complex` | |
| 1024 px checkerboards (12 / 24 px squares) | 22 / 23 ms → `too_complex` | |
| crafted stripes, rings, comb, serpentine (over the budget) | 23–40 ms → `too_complex` | |
| worst crafted input found under the budget (2 px diagonal lines, rings) | about 130 ms | |

A debug build takes about 0.09 s (512), 0.3 s (1024) and 0.6 s (2048) for the same
PNGs. The image crates are built with `opt-level = 3` even in dev (root `Cargo.toml`),
which keeps the debug test suite and the debug e2e server usable.

## Pipeline (SVG)

SVG input never reaches the output as markup: it is reduced to geometry and emitted
through the same fixed template. `svg_in.rs` runs these steps, each before the next
gets a chance to be expensive:

1. **Bytes.** Over 512 KiB is `too_large`; not UTF-8 is `invalid_svg`.
2. **Depth scan.** A recursion-free byte scan bounds element nesting at 64
   (`too_complex`) before roxmltree's recursive tokenizer runs: about 5,000 nested tags
   overflow a 2 MiB stack (about 200 in a debug build) and abort the process. Quotes in
   tags and comments can't hide nesting from it, and it reads the DOCTYPE with roxmltree
   0.21.1's own grammar (quoted literals in the external ID and entity values, comments
   and PIs skipped whole; `<!ATTLIST`, `<!ELEMENT` and `<!NOTATION` end at their first
   `>`, quoted or not, as roxmltree ends them). A DOCTYPE or `<!` declaration it can't
   follow is `invalid_xml` (roxmltree rejects those too). Skipping more than roxmltree
   would hide elements: a quoted `[` once made the scan swallow the whole file.
3. **Entities only as Illustrator writes them.** Illustrator's Save As SVG declares its
   namespace URIs as entities (`<!ENTITY ns_ai "http://ns.adobe.com/…">`). Every
   `<!ENTITY` in the file must be exactly `<!ENTITY name "value">`: at most 32, values of
   at most 256 bytes with no `&` (no nesting, so no billion laughs), `%` or `<`, no
   `SYSTEM`/`PUBLIC` (no XXE), and all references together may expand to at most 256 KiB.
   Anything else is `invalid_xml`, with advice to export a plain SVG.
4. **roxmltree** with a 20,000-node limit (`invalid_xml` over it, or when malformed).
5. **Prescan** (`svg_scan.rs`, iterative): counts, lint facts, the CSS cost and the
   reference graphs; see [below](#the-svg-prescan).
6. **usvg** turns shapes, transforms, CSS, `<use>` and the viewBox into plain paths.
   Its image resolvers are replaced with no-ops: the defaults **read local files** named
   by `<image href>` (relative to the working directory when there is no base directory)
   and decode embedded rasters. usvg is built without text, fonts, svgz or the writer.
7. **Template filter and budgets** (`paint.rs`, which reports `outside_canvas` from
   the painted ink after its clip paths): what the drawing is (below), and
   path segments ≤ 5,000 (strokes count 5), outline travel ≤ 40,000 units, dashes ≤
   20,000, clipped groups ≤ 16 (`too_complex` otherwise).
8. **Truth raster.** A bounded tiny-skia painter (not resvg) paints the drawing at
   512 px: fills and stroke outlines in order, group opacity folded into each paint, clip
   paths as masks (including those of the groups around `draw-here`); filters are
   ignored, masked content is painted unmasked, gradients become their stops' average
   colour and patterns black. When the drawing uses more than one colour, near-white
   paints (luma ≥ 230 of 255, 0.9) are cut-outs, the way 27 catalog files draw white
   eyes: each paint is drawn in black or white, and a pixel is ink when it is at least
   half opaque and at least half black.
9. **Vector or retrace.** The vector candidates are every painted fill and stroke
   outline concatenated, rendered with nonzero and with even-odd (white details drawn on
   top of a dark shape become holes); each shape turned to wind the same way by its
   signed area (separate shapes that overlap with opposite winding would otherwise
   cancel); and the same two without the light paints (white drawn under dark shapes).
   Each is compared with the truth at 512 px. A differing pixel is **edge noise** when
   both renders have an edge in its 3×3 neighbourhood (anti-aliasing along an outline
   they share), and **hard** otherwise (a hole one has and the other doesn't, an extra
   speck). A candidate qualifies with at most 0.1% of the pixels differing and at most
   13 hard pixels (half the 1 unit² speck the trace removes), so no detail the retrace
   would keep can be lost or added; the first with no hard pixel wins, else the one
   with the fewest. Its contours wholly outside the square are dropped (they change
   nothing inside it), and it is emitted as is (`vector_exact`) if what is left stays
   within 0.5 units of the square. Otherwise (layered details such as white, black,
   white, small details no fill rule reproduces, overlaps in an even-odd shape, clips
   that cut through shapes, opacity, a shape crossing the edge of the square, which Fit
   would scale into view), the truth is painted again at the 1024 px trace grid and
   traced (`retraced`), with splines spliced at 10° instead of the 15° used for
   drawings, so long gentle curves stay within about 0.1 unit.
10. **Fixes, emit, metrics, lints**, as for rasters.

### The template filter (SVG)

The SVG template has layers `reference-*` (hidden), `draw-here` and `guides` (visible),
and every guide and reference uses a template colour (`palette.rs`):

- If a group with id `draw-here` contains anything, only it is the drawing. (Ids match
  loosely: `Draw_here`, `draw here`, `draw-here-2`.) Clip paths of the groups around
  it still apply (Figma wraps every frame in one). If other layers have visible ink
  (not a template colour, not near-white, at least half opaque), the
  `outside_draw_here_ignored` tip says they were left out.
- Otherwise (no such layer, or the artist drew on a new layer) the whole document is,
  minus groups with id `guides` or `reference-*`.
- Either way, a fill or stroke in exactly a template colour is dropped: Figma and
  some exporters strip ids but keep colours.
- Hidden content (`display:none`, `visibility:hidden`) never reaches the filter: usvg
  drops it.
- `guides_visible` fires when the filter dropped something visible: a guides or
  reference layer, or a template-coloured paint. Not when `draw-here` was used alone: the
  guides didn't get in the way then. An empty template is `empty` with
  `guides_visible`, whose message asks for a drawing on the "Draw here" layer.

### The SVG prescan

usvg (and simplecss, its CSS engine) have no limits of their own on depth, expansion or
work, and a thread that overflows its stack aborts the whole process: `catch_unwind`
can't catch that. So before usvg runs:

- **Counts.** Nesting ≤ 64, nodes ≤ 20,000, `<use>` ≤ 500, definitions (clip paths,
  masks, patterns, markers, symbols, gradients, filters) ≤ 64.
- **Reference graphs.** usvg's parser copies every `<use>` target into the tree; its
  converter then follows references (`fill`/`stroke` patterns, `clip-path`, `mask`,
  `filter` and `feImage`, markers, `<use>`), converting the target's content nested
  inside the referencing element. References come from attributes, `style` attributes
  and `<style>` sheets (matched with simplecss and the same element view as usvg), and
  `fill`, `stroke` and markers are inherited. Every candidate value counts, not just the
  cascade winner, so the graphs are a superset of what usvg follows. Matching usvg
  exactly matters: each of these made a loop invisible to an earlier version of the
  prescan, and each aborts usvg on a 64 MiB stack, so each is a test:
  - ids and references are read the way svgtypes reads them (an id written with a
    trailing `&#9;` keeps the tab), and both `id` and `xml:id` define ids;
  - presentation attributes count in any namespace (usvg reads `xml:fill`);
  - every `<style>` element counts, whatever its namespace;
  - `inherit` takes the parent's value, also for `clip-path`, `mask` and `filter`.

  A differential fuzz (not checked in) generated 165,000 random reference structures
  over these forms (patterns, masks, clip paths, markers, symbols, `feImage` filters and
  `<use>`; attributes, `style`, classes, `xml:` attributes, `inherit`, tab ids, wrapped
  definitions): the 71,000 the prescan accepted all ran through `process_on_big_stack`
  without aborting. The loop check is conservative: usvg would have survived all of a
  sample of 200 rejected loops (it breaks some loops itself, and never converts
  definitions nothing uses). Real artwork has no loops at all.

  From the graphs:
  - **Cycles** are rejected. usvg only breaks one- and two-step cycles; a three-pattern
    cycle, a pattern whose content inherits a fill pointing back at it, or the same
    loop made with CSS classes recurses forever and **aborts the process even on a
    64 MiB stack**, from a 600-byte file (verified against usvg 0.48.1 for patterns,
    masks, clip paths, `feImage` filters, CSS and inherited fills).
  - **Nesting** along reference chains is bounded at 4,096 levels. The per-element caps
    multiply: 64 patterns each 60 groups deep is about 3,970 levels although no element
    is nested more than 64 deep. That is within the budget, and it needs a big stack
    (next section).
  - **Expansion** is bounded at 20,000 elements on top of the 20,000 nodes: every
    `<use>` copies its target, every path with an objectBoundingBox pattern gets its own
    copy of the pattern's content, and every vertex its own marker.
- **CSS cost**, an upper bound on simplecss's steps, ≤ 20,000,000 before simplecss
  runs. simplecss computes a line and column from the start of the text whenever a
  declaration ends, so parsing is quadratic: 16,000 declarations (144 KB) take 1.2 s
  in release. And usvg matches every rule against every element and `<use>` copy, with
  a descendant combinator backtracking through every ancestor: `x g g g g g g g g g g`
  over 60 nested groups is about 10^12 steps. Selectors may have at most 32 parts.

### Running it: the stack

`process_upload` is plain synchronous code, but **it needs a big stack for untrusted
input**: the deepest SVG the limits allow makes usvg recurse about 4,000 levels.
Measured in release on usvg alone, the worst accepted chains (64 patterns × 60 groups,
64 masks × 60 groups, 33 pattern/`<use>` pairs × 58 groups) abort the process on 2 MiB
and 4 MiB stacks and pass on 8 MiB. Tokio's worker and `spawn_blocking` threads have
2 MiB, and raising the runtime-wide `thread_stack_size` would multiply by up to 512
blocking threads, so instead:

```rust
pub fn process_on_big_stack(bytes: Vec<u8>, limits: &Limits, fixes: &[Fix],
    permit: impl Send + 'static) -> tokio::sync::oneshot::Receiver<Result<CleanShape, ProcessError>>
```

spawns a named `design-kit` thread with a 64 MiB stack (`PROCESS_STACK_BYTES`; only the
touched pages are committed), moves `permit` (an `OwnedSemaphorePermit`) into it and
drops it when the work ends, before sending the result, so a caller that stops waiting
doesn't free the CPU slot while the work goes on (a unit test holds the work open,
drops the receiver and checks the permit is still held). A panic, or failing to start
the thread, is `internal`. Every SVG test runs through it, and the catalog corpus runs on
threads with the same stack, so a regression fails a test instead of aborting the test
binary.

### Pinned versions

The prescan mirrors other crates' code: usvg 0.48.1's reference following, simplecss
0.2.2's selector matching, svgtypes 0.16.1's IRI parsing and roxmltree 0.21.1's
tokenizer. A reference loop it misses aborts the process. So `usvg` and `simplecss` (and
the test-only `resvg`, which would otherwise pull a second usvg) are pinned with `=` in
`server/Cargo.toml`, and `svg_parsing_crates_are_the_audited_versions` fails when
`Cargo.lock` holds any other version (or a second copy) of the four. Bumping one means
re-reading `svg_scan.rs` against its new sources first.

### Hostile SVGs

Each has a test with its exact outcome (`server/tests/design_kit_svg.rs`). Times are
release, median of 9, through `process_on_big_stack`.

| Input | Outcome | Time |
|---|---|---|
| `<script>`, `on*=` handlers, `<foreignObject>`, `<set href>`, `javascript:` and external links, `@import` | Ok, `active_content_removed`; the output is one path | |
| `<image href>` naming a local file (a temp file: usvg's default resolver loads it, ours doesn't) | Ok, `image_ignored`; not read | |
| XXE, billion laughs, parameter entities, markup in an entity, entity references expanding past 256 KiB | `invalid_xml`, with advice to export a plain SVG | 0.1 ms |
| Illustrator's Save As header (8 namespace entities, a `<switch>` with a `requiredExtensions` foreignObject and private data) | Ok, nothing reported as removed | 1.5 ms |
| 256-byte entity referenced 1,000 times (at the expansion cap) | Ok | 1.2 ms |
| 5,000 nested `<g>`; 200 hidden behind quoted `/>` and comments; 19,000 behind a DOCTYPE with a quoted `[` or `>` in an `ATTLIST` | `too_complex` | 0.1 ms, 0.2 ms (19,000) |
| A DOCTYPE or `<!` declaration roxmltree can't parse | `invalid_xml` | |
| 64 patterns or masks × 60 groups; 33 pattern/`<use>` pairs × 58 groups | Ok (about 3,600-3,970 levels deep) | 4-5 ms, 12 MB peak RSS |
| 60 pattern/`<use>` pairs × 58 groups | `too_complex` (nesting) | 1 ms |
| three-step pattern, mask, clip, `feImage` and `<use>` cycles; loops made with inherited fills, CSS classes, `style` attributes, `xml:id`, `xml:fill`, a stylesheet in another namespace, `clip-path: inherit` or ids ending in a tab | `too_complex` (loop) | 0.1 ms |
| 2,000-long clip, mask or pattern chain | `too_complex` (definitions) | |
| `<use>` bombs (2^25 copies; 400 × 100; 501 uses), pattern fan-out, marker per vertex | `too_complex` (expansion) | 0.1 ms |
| `<filter>` with 4,000 morphology and blur primitives | Ok, `filters_ignored` | 2.8 ms |
| 8 nested oversized masks (949 MB with resvg) | Ok, `clip_or_mask` (masks ignored) | 0.6 ms |
| backtracking CSS selector; 16,000 declarations (sheet or `style` attribute); 10,000 rules × 3,000 elements; a long `style` attribute copied by 400 `<use>`s | `too_complex` (CSS) | 0.1 ms, 0.4 ms (first two) |
| 4,990 segments crossing the drawing | `too_complex` (outline travel) | 0.5 ms (350 ms before the budget) |
| 1,200 segments crossing the drawing (the most the budget allows) | Ok | 64 ms |
| 0.001-unit dashes; 20 nested clipped groups; 6,000 segments | `too_complex` | |
| 25,000 elements | `invalid_xml` (node limit) | |
| Latin-1 bytes | `invalid_svg` | |
| gzip (`.svgz`), UTF-16 (with or without a BOM) | `unsupported_format` (`svgz`, `utf16`), with advice to save a plain UTF-8 SVG | |
| 600 KiB | `too_large` | 0.1 ms |
| HTML that mentions `<svg>` | `invalid_svg` (root) | |

### Cost (release, measured on the dev VM)

| Input | Time | Peak RSS (whole process) |
|---|---|---|
| official SVGs, `vector_exact` (168) | median 1.2 ms, max 5.6 ms | 4.7 MB (smile) |
| official SVGs, `retraced` (16) | median 41 ms, max 48 ms | 17 MB |
| 16 nested clipped groups, retraced | 54 ms | 24 MB |

A debug build takes about 17 ms and 250 ms for the same official SVGs (usvg, simplecss,
roxmltree, svgtypes and kurbo are built with `opt-level = 3` in dev too).

### The catalog corpus

`server/tests/design_kit_catalog.rs` uploads every one of the 184 official SVGs as is.
Each must succeed, match an independent render (resvg draws the original in the board's
wrapper with colours classified the same way; IoU ≥ 0.99), match the investigation's
rsvg metrics (`catalog/metrics_summary.csv`) within 2 points for fill and left edge when
single-colour, lose or add no detail, and get no warning for its kind.

IoU alone can't see a lost detail: a 50 unit² eye is under 1% of a head. So every ink
region and every background region of the oracle and of the output is a detail, with a
core of the pixels whose 8 neighbours are in it too (a sliver one pixel thick has none);
each detail whose core covers at least half a unit² must keep at least half its core in
the other render. As a check of the check: with the vector path loosened to take the
first candidate within 3% of the pixels, it flags 28 assets, three of which (the bull
and fang heads, the mystic-moon tail) still score IoU ≥ 0.99.

All pass: 168 `vector_exact`, 16 `retraced` (the 13 multi-colour heads and tails whose
details are layered, and jackolantern, replit-notmark and shades, whose shapes cross the
edge of the square), worst IoU 0.992. One asset legitimately trips a rule and is listed,
with the exact warning it must get, in the test's `EXCEPTIONS`: the trans-rights-scarf
head's middle stripe is explicit white and crosses the neck, so as one colour the head
has a 20-unit notch there (left edge 80%) and gets `neck_gap`.

The oracle classifies colours, not rendered pixels: ink is at least half opaque and at
least half black after recolouring near-white to white and everything else to black.
Classifying pixels by luma instead counts an edge pixel with 10% dark paint over a white
detail as ink, which moves every cut-out edge about 0.4 px and scored even an exact
conversion of the ghost head at 0.977.

Findings from the corpus changed the pipeline, not the thresholds:

- The truth painter had the same pixel-luma bias (ghost: 349 px off at 512, so it was
  retraced); it now paints each paint black or white and resolves edges at 50%.
- fang and others drew separate shapes that overlap with opposite winding, and white
  eyes under the dark shape; the winding-normalised and dark-only candidates made them
  `vector_exact` (140 → 171), which the reference shapes need. Later, five turned out to
  reach past the square; nr-rocket and nr-booster stay exact once their contours wholly
  outside it are dropped, and the three that cross its edge are retraced (168).
- Retraced curves were up to 0.5 units off (IoU 0.976-0.99) with the drawing tracer's
  then 45° splice threshold (drawings now use 15°); the SVG retrace now uses 10°.
- The direction lint called that scarf head rotated (`faces_up_down`) because its top
  and bottom are full width. A quarter turn puts the neck on the top or bottom and the
  front opposite, and a front is never that full, so when both are full neither counts
  as a turned neck, and the gap is reported as `neck_gap`.

### Reference shapes

`design_kit::refs::REFS` is a static table of the 9 heads and 9 tails the studio pairs
uploads with (Standard and free: heads default, beluga, bendr, evil, fang, smile, pixel,
sand-worm, tongue; tails default, curled, bolt, round-bum, hook, block-bum, sharp,
pixel, freckled), with catalog display names. Nothing is processed at runtime.
`server/tests/design_kit_refs.rs` re-processes the raw SVGs vendored in
`fixtures/design_kit/refs/` and checks the table equals the output, is `vector_exact`
and has no warnings; a unit test in `customizations::catalog` checks the names, group
and price. To regenerate:

```
cargo test -p arena --test design_kit_refs -- --ignored --nocapture print_refs_table
```

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
- when the reference is visible, also the product of each guide colour and the ghost,
  or the segment from it to the ghost. Four of the five products are blues and
  crimsons dark enough to be ink (luma 64–99; the grid's, a light blue-grey at 170, is
  never ink), so without a visible reference they are ordinary ink.

The reference is **visible** when at least 1% of the canvas is inside it: solid pixels
within 24 of the ghost, visibly coloured (chroma ≥ 12), whose whole 3 × 3 neighbourhood
is too. A visible reference covers tens of percent. Colour alone would not do: light
greys from about 190 to 213 (a soft black edge, a light-grey background) are within 24
of the ghost, and so is part of the anti-aliased edge of a dark-blue drawing on white
(about 0.1% of the canvas at 1000 px). The chroma floor rules out the greys, and the
edge is too thin to have an inside.

So a navy or crimson drawing works with crisp or soft edges, on white or light grey,
and filled inside a black outline. Ink close to the products is dropped only when the
reference is visible (it can't be told apart from guides over the reference), or on
paper close to the ghost's colour (a cool lavender-grey), which looks like a reference
covering the whole canvas.

Then:

- **Alpha raster** (at least 0.5% of pixels transparent and 0.1% opaque): coverage is
  the pixel's alpha. Near-white pixels are not ink either, so white details painted on
  the shape (eyes, stripes) become holes, as they would in an opaque export.
- **Opaque raster** (everything else, including every JPEG): composite over white;
  coverage is the darkness `255 − luma` (Rec. 709), so after the 50% threshold ink is
  luma < 0.5.

Info lints from the same pass:

- `guides_visible`: at least 0.1% of pixels are within distance 24 of a guide colour
  (or of a product, when the reference is visible) *and* visibly coloured (chroma ≥ 12;
  the ghost has 18), or are inside a reference as defined above. The chroma check keeps
  grey anti-aliasing of black ink, pencil and paper from counting. "We ignored the
  template guides. Hide them next time for the cleanest result."
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
12 catalog samples, measured with it, match the rsvg-derived
`catalog/metrics_detail.csv` for every metric a lint reads: fill% and edge% within 0.5
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
| `faces_left` | the drawing's left side < 85%, its right side ≥ 85% and its left < 60%; or no side qualifies and centroid x > 52 | warn, offers **Flip** | head | heads' right edge max 47.5%, centroid x max 48.5 |
| `faces_up_down` | the drawing's left side < 85%, its top (bottom) ≥ 85% and its bottom (top) < 60%; or its left side is full height and its right side > 60% (not a solid square) | warn | head | heads' left edge never < 88%, right edge max 47.5% |
| `tail_reversed` | the drawing's left side < 85%, and its right, top or bottom ≥ 85% with the side opposite < 60% | warn, offers **Flip** when it's the right side | tail | tails' left edge never < 88%; 95% of tips ≤ 49.5% |
| `outline_only` | hole fraction > 55% | warn | both | catalog max 47.1%; a 6-unit stroke outline is ≈ 77% |
| `specks_removed` | pieces or holes < 1 unit² removed | info | – | removed automatically |
| `colours_flattened` | see the ink rule | info | – | |
| `guides_visible` | see the ink rule | info | – | |
| `outside_draw_here_ignored` | SVG `draw-here` was used alone and other layers had visible ink | tip | – | |
| `semi_transparent` | see the ink rule | info | – | |
| `non_square` | canvas not square | tip | – | |
| `low_resolution` | longer side < 128 px | tip | – | |
| `image_ignored` | SVG `<image>`s (never loaded) | tip | – | |
| `text_ignored` | SVG text not converted to outlines | tip | – | |
| `strokes_converted` | SVG strokes became filled outlines | info | – | |
| `gradient` | SVG gradients became their average colour (light ones cut-outs), patterns solid | info | – | |
| `clip_or_mask` | SVG clip paths applied and/or masks ignored; the message names only what happened (`clipped`, `masked`) | info | – | |
| `filters_ignored` | SVG filters | info | – | |
| `active_content_removed` | SVG scripts, handlers, embedded HTML, animations, external links | info | – | |
| `outside_canvas` | SVG ink (at least half opaque, after its clip paths) reaches more than 0.5 units outside the square, whichever the strategy | info | – | |

For SVG input, `colours_flattened` fires when the drawing uses more than one colour or
one clearly coloured one (chroma ≥ 64), `semi_transparent` when a paint is less than
fully opaque, `non_square` when the canvas (after viewBox, width and height) isn't
square, and `guides_visible` as described in [the template filter](#the-template-filter-svg).

Direction is judged by where the drawing's full-height side is (`drawing_edges`), not
by the square's edges or the centre of mass alone. A short or padded drawing whose
left side is full height isn't mistaken for a rotated one, a quarter turn of a
top-heavy head (beluga's centre of mass moves to x 53.5) isn't mistaken for a mirror, and
a mirrored head whose mass is near the middle (guitar, x 51.5) still gets Flip. The
centroid rule only applies when no side qualifies.

A full-height side other than the left only says the drawing is turned or mirrored when
the side opposite it looks like a front or tip (< 60%; every official head's front is at
most 47.5% full). Many correct heads also have a full top and bottom (`default`, the
template's reference, and `pixel` and `sand-worm`), and blocks are full on every side,
so for them a left side under 85% is a neck with a gap in it, not a rotation: they get
`neck_gap` and no Flip. The rule can't tell a head with one flat side (top or bottom),
a rounded other side and a notched neck from a rotated head; that case still reads as
rotated. A turned or mirrored tail whose tip (the side opposite its full-height side)
is at least 60% full isn't recognised as one; it gets `neck_gap` if its left side is
under 85%.

Fit keeps the aspect ratio: it scales the larger side to 100, left-anchors the drawing
and centres it vertically. So Fit is offered only while it would clear the margins or
at least move the drawing to the neck edge or make it noticeably bigger; a fitted wide
drawing gets the `margins` warning without Fit, asking for a taller drawing ("Draw it
taller, from edge to edge").

So that each problem gets one message and one fix:

- `margins` suppresses `neck_gap` when it offers Fit and the fitted drawing would have
  a full-height left edge (the drawing's own left side × height / larger side ≥ 85%).
  A small dot or a wide, short drawing keeps both.
- A direction lint suppresses `neck_gap` when it found the full-height side elsewhere
  (opposite a front or tip, as above).
  A drawing whose top and bottom are both full width but whose left side has a gap
  (the trans-rights-scarf head, as one colour) gets `neck_gap`.
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
`internal` (a caught tracer, usvg or painter panic, a processing thread that couldn't
start, or an internal invariant that didn't hold) is a bug: report it as a 500. The
trace budget and the SVG prescan keep crafted uploads clear of the panics and aborts
we know about (see above). A decoder panic is caught too, but is `invalid_image`: the
file is one our decoders can't read.

Not everything is caught. A panic elsewhere in our own code unwinds out of
`process_upload` (`process_on_big_stack` catches it and returns `internal`), and a
failed allocation aborts the process (Rust's allocator does not return an error).
Callers must isolate it: PR 3 runs it in a child process with a memory limit. A
caught panic still runs the panic hook, so it reaches Sentry in a process that
initialises it. The zune-jpeg panic above needs only a tiny crafted JPEG, so anyone
can trigger one per request: rate-limit or de-duplicate those reports, or don't
report panics from the decode process.

| Code | When |
|---|---|
| `empty_file` | zero bytes |
| `unknown_format` | not PNG, JPEG or SVG, and not a format we recognise |
| `unsupported_format` | HEIC, GIF, WebP, PSD, ZIP/.procreate, PDF/.ai, gzip (.svgz), UTF-16 text |
| `too_large` | over the byte cap for the format |
| `image_too_large` | wider or taller than 2048 px, or a progressive or multi-scan JPEG over its pixel cap (about 1448 px square for colour, 1254 for CMYK) |
| `invalid_image` | truncated, corrupt or zero-size |
| `invalid_xml` | malformed SVG or DOCTYPE, over 20,000 nodes, or declaring entities other than Illustrator-style plain text (its own message: export a plain SVG) |
| `invalid_svg` | not UTF-8, the root isn't `<svg>`, or usvg can't read it |
| `too_complex` | over the trace budget, or `d` over 64 KiB (noise, checkerboards, photos, crafted stripes); for SVGs, over a prescan or painting budget (nesting, loops, expansion, CSS, segments, outline travel, dashes, clips) |
| `empty` | nothing drawable (carries info lints, e.g. guides only) |
| `internal` | a bug |

## The studio

`GET /customizations/studio` (`server/src/routes/studio/page.rs`) is a public page that
needs no login and stores nothing on the server. Every preview board is server-rendered
with `components::snake_board`, with the default head and tail in place; the client,
`server/static/studio.js`, posts uploads and then only sets attributes on placeholders:

| Placeholder | What the page's JS sets |
|---|---|
| `path.studio-head`, `path.studio-tail` (every board: 16 live-loop frames, All directions, two Game size boards) | `d` and `fill-rule` of whatever fills that slot: the upload, or the "Pair with" reference |
| `path#studio-closeup-path`, `g#studio-gaps` | the active slot, and red brackets on its `metrics.left_edge_gaps` |
| `#studio` | `--studio-snake`, the snake colour (`.studio-board` CSS reads it) |
| `.studio-board` | `light` / `dark` |
| `option[data-d][data-fill-rule]` in `#studio-pair-head` / `#studio-pair-tail` | read only; JS adds `option[value=user]` ("Your head"/"Your tail") once that slot holds an upload |

Every `d` and colour is checked (`/^[MLQCZ0-9 .\-]*$/`, `/^#[0-9a-f]{6}$/i`) before use,
including what `localStorage` (`arena:studio:v1`) restores, and lint text goes in with
`textContent`.

- **Upload as: Head / Tail** (above the drop zone) picks the slot the next upload fills
  and the slot the close-up and the checks show; switching says which in the status line.
- **Moving a result.** Right after an upload, "Use it as a tail instead" moves the
  result to the other slot without re-posting (both kinds' lints are in the response)
  and puts back what it replaced. Tapping Tail while the tail slot is empty offers "Use
  the file you just uploaded as your tail" instead, so either way of correcting a
  mislabelled upload works, and an upload is never moved over the other slot's upload by
  accident.
- **Flip and Fit always work.** The uploaded file stays in memory only; once it's gone
  (a reload, another upload), they re-post the saved path as the downloadable SVG.
  Both fixes are rewrites of the clean path, so the result matches fixing the original
  (a route test checks the metrics agree within 1%); the original file's notes are kept.
- **A failed request changes nothing**: the last result, and its buttons, stay.
- **The instant format check** before uploading comes from the server: `#studio-drop`
  carries `data-sniff`, the rejected entries of `design_kit::SIGNATURES` with their
  advice and the size limit. Anything else is posted and the server decides.
- After a result, the drop zone shrinks to one row and the page scrolls to the preview
  (with the checks beside it, at 980px+) or to the status line.

### The endpoint and its guards

`POST /customizations/studio/process?fix=flip&fix=fit` takes the raw file as the body
(`server/src/routes/studio/process.rs`). It never touches the database (no
`PageFactory`, `OptionalUser` or session), every response is `Cache-Control: no-store`,
and the log line (`event_type="studio_processed"`) holds the outcome, input format,
strategy, lint codes, fixes, size and duration, never content or IPs. In order:

| Guard | Limit | When exceeded |
|---|---|---|
| Upload slots (route middleware, `try_acquire` before anything reads the body) | 3 requests | 503 `busy` |
| Global token bucket (header-free: `X-Forwarded-For` can be spoofed on `run.app`) | burst 20, 1/s | 429 `rate_limited` |
| Body (`DefaultBodyLimit`) | 4 MiB | 413 `too_large` |
| Body pace (`BodyPace`): all of it within 30 s, and after 5 s at least 16 KiB/s on average, so a stalled or trickling client is cut off in seconds instead of holding an upload slot | | 408 `upload_timeout` |
| Processing slot (like `BACKUP_SLOT` in `backup.rs`), held until the worker exits | 1, waited for up to 3 s | 503 `busy` |
| The worker: CPU, memory, wall clock (below) | | 422 `too_complex` or 503 `busy` |

| Outcome | Response |
|---|---|
| a clean shape | 200 `{path_d, fill_rule, strategy, input, metrics, lints: {head, tail}, info}` (the page builds the SVG file from `path_d` and `fill_rule`) |
| a `ProcessError` caused by the upload | 422 `{error: {code, message}}` |
| the worker died (a crash, a resource limit, the OOM killer) | 422 `too_complex`, logged at warn with the exit status |
| no answer before the deadline (the worker is killed) | 503 `busy`, logged at warn |
| `internal`, or a worker reply that fails its re-check | 500 |

### The worker process

usvg recursing past the prescan would abort the whole server (no stack is big enough
for a reference loop), so every upload is processed in a fresh child:
`arena studio-worker [--fix=flip] [--fix=fit]` (`arena::studio_worker`). `main.rs`
dispatches to it before Sentry, config, telemetry or the database. It reads the file on
stdin, runs `process_on_big_stack`, writes one JSON reply to stdout and exits:

| Exit | Meaning |
|---|---|
| 0 | `{"shape": ...}` or `{"error": {code, message}}` on stdout |
| 64 | bad arguments |
| 70 | `{"internal": ...}` on stdout: a bug |
| 74 | stdin or stdout failed |
| a signal | crashed: stack overflow or failed allocation (SIGABRT), CPU limit (SIGXCPU), killed |

The server spawns it from `/proc/self/exe` on Linux (its own executable, even if the
file was replaced since, so the worker is always the same build) with
`tokio::process::Command` (`kill_on_drop`, an empty environment, stdin written while
stdout and stderr are read, both capped), re-checks the reply (the path alphabet and
the size limit), and sets these between fork and exec (`pre_exec`; plain system-call
wrappers only). The processing slot is held until the worker has exited and been
reaped (`Worker::run_holding`), or until it is killed when the request goes away.

| Limit | Release | Debug | Why |
|---|---|---|---|
| `RLIMIT_CPU` (soft; hard is +1 s) | 5 s | 30 s | the slowest accepted uploads take 0.25 s in release, 7 s in debug |
| `RLIMIT_DATA` | 192 MiB | 192 MiB | the heaviest legitimate uploads need 88 MiB (below) |
| `RLIMIT_AS` | 320 MiB | 320 MiB | the second memory cap, for gVisor (below); legitimate uploads need at most 160 MiB |
| `RLIMIT_CORE` | 0 | 0 | a crash never writes a core file |
| nice | 19 | 19 | the lowest CPU priority: on the one vCPU, live games always run first |
| `oom_score_adj` | 1000 | 1000 | if memory runs out, a Linux kernel kills the worker, not the server |
| wall clock (then SIGKILL) | 10 s | 45 s | a slow answer means a busy machine (the CPU limit catches big uploads first) |

**Run the service on Cloud Run's second-generation execution environment.** The
memory protection above assumes a Linux kernel: there `RLIMIT_DATA` covers every private
writable mapping, and `oom_score_adj` makes the OOM killer pick the worker. The first
generation runs on gVisor, which applies `RLIMIT_DATA` to `brk` only (not `mmap`, which
is where big allocations and the 64 MiB stack live; gVisor `mm/syscalls.go`, issue 156)
and kills the whole sandbox, server and games included, when the instance runs out of
memory. `RLIMIT_AS`, which gVisor does enforce on `mmap`, keeps a bomb there to about
200 MiB, but only the second generation makes the OOM killer pick the worker.
`tf-arena` sets no execution environment yet: pin
`execution_environment = "EXECUTION_ENVIRONMENT_GEN2"` before the studio is linked.

`RLIMIT_DATA` counts private writable mappings, so the 64 MiB processing stack counts in
full (only its touched pages use memory). Measured on the dev VM with
`prlimit --data=N arena studio-worker < file` (debug build; peak memory is the same in
release):

| Upload | Fails at | Passes at | Release time |
|---|---|---|---|
| 2048 px PNG, 2048 px JPEG | 80 MiB | 88 MiB | 93 ms, 78 ms |
| 2048 px black and white noise PNG (accepted: a 38 KB path; 3.8 s in debug) | | | 161 ms |
| 1448 px progressive JPEG noise (the progressive cap), 1254 px progressive CMYK JPEG | 80 MiB | 88 MiB | 237 ms, 226 ms |
| 16 nested clipped groups, retraced | 80 MiB | 88 MiB | 63 ms |
| 361 white dots on black (`too_complex`) | 80 MiB | 88 MiB | 75 ms |
| 64 patterns or masks x 60 groups, 33 pattern/`<use>` pairs x 58 groups (deepest nesting) | | 72 MiB | 6 ms |

192 MiB is twice the worst legitimate need; a memory bomb can then touch at most about
128 MiB of heap, which fits in what a 512 MiB instance has free beside the server
(about 320 MiB idle), so the data limit stops it before the OOM killer has to.

`RLIMIT_AS` counts every mapping: the executable, the main stack and a malloc arena's
reservation come to about 125 MiB before any work (debug build; release is smaller).
With `prlimit --as`, the 2048 px uploads and progressive JPEGs need 144 MiB, the deepest
reference chains 160 MiB, and the 4,900-element SVG that is rejected as too complex 224
MiB. 320 MiB (the data limit plus 128 MiB) never binds before `RLIMIT_DATA` on Linux.

The integration tests run the real binary (`server/tests/studio_worker.rs`, Linux only):
the limits, nice value, `oom_score_adj` and empty environment as `/proc` shows them; an
abort, a memory bomb (under the data limit, and under the address-space limit alone), a
CPU hog and a hang; the heaviest legitimate SVGs under the default limits; and the
processing slot held until the worker is gone, including when the request is dropped.

## Tests

- `server/tests/design_kit_raster.rs`: sniffing; round trips of every vendored catalog
  head and tail rendered with resvg at 512, 1000 (the template's size), 1024 and 2048
  px, transparent and opaque, and of some roughened (wobble, blur, grain, specks) and
  JPEG q80 (IoU ≥ 0.97 against the original, outline within 1.25 units of the
  original's, or ¼ unit for the straight-edged samples, left edge ≥ 95%, no warnings);
  samples exported at 1200 and 1536 px (a non-integer downscale) with `d` at most 1.6
  times its size at 1024; a navy head at six export sizes; navy and crimson drawings, crisp and soft-edged, on
  light grey and inside a soft black outline; EXIF orientation; the progressive JPEG cap
  (including a libjpeg file whose first scan carries every component), the multi-scan
  cap and a JPEG the decoder panics on; skipped PNG metadata; the ink matrix; one case
  per lint with exact codes for each kind, including a rotated top-heavy head, a
  mirrored centred head, notched necks on flat-topped heads and tails, short and wide
  drawings before and after Fit, and combined fixes; suppression and pass state; fixes
  (including the inverse transform); hostile rasters with exact errors (including
  truncated JPEGs and crafted stripes, rings, a comb and a serpentine stopped by the
  trace budget); and a property test that every output is one clean path.
- `server/tests/design_kit_metrics.rs`: the metric oracle described above.
- `server/tests/design_kit_svg.rs`: real-world exports (Illustrator with a DOCTYPE, CSS
  and a 1000-unit viewBox; Illustrator's Save As with namespace entities and a
  `<switch>`, `fixtures/design_kit/illustrator/`; shapes and transforms without a
  viewBox; Inkscape even-odd holes; Figma frame clips; overlapping shapes of opposite
  winding; white details on, under and beside dark shapes; layered details that must be
  retraced); small holes and marks (a 3×3 even-odd hole, an r = 1.7 eye, 2×2 nostrils
  beside a 3×3 sparkle) that the vector path must not lose; geometry off the square (a
  stray shape dropped so Fit works, a shape crossing the edge retraced, a frame clip
  that already cut the overflow); every input fact; fixes on SVG input; the template
  (empty, a head in `draw-here`, ids stripped with guides and references visible, a
  drawing on a new layer, other layers left out beside `draw-here`, a clip around
  `draw-here`); the hostile table above, with exact outcomes; the big-stack runner (the
  permit, and the raster worst cases giving the same answers on it); and the pinned
  versions of the crates the prescan mirrors. Unit tests in `mod.rs` check that the
  permit is held until the work ends even when the caller stops waiting, and that a
  thread that can't start is `internal`; in `svg_scan.rs`, that the depth scan reads
  DOCTYPEs as roxmltree does.
- `server/tests/design_kit_catalog.rs`: the 184-file corpus described above.
- `server/tests/design_kit_refs.rs`: the reference table against fresh processing.
- `server/tests/studio_worker.rs` (Linux only): the real `arena studio-worker` binary:
  PNG, JPEG and SVG replies equal in-process processing, fixes reach the worker, user
  errors come back as rejections, and an abort, a memory bomb (the production data
  limit, and the address-space limit alone, as under gVisor), real processing at a
  tight data limit, a CPU hog and a hang each end only the child with the outcome the
  endpoint maps (the hang test also reads `/proc` for the limits, the nice value,
  `oom_score_adj`, the empty environment and that the killed worker was reaped); the
  heaviest legitimate SVGs under the default limits; the processing slot held until the
  worker is gone, and freed (with the worker killed) when the request is dropped; the
  CLI protocol and exit codes.
- `server/src/studio_worker.rs` (unit): the JSON shapes, reply re-checks, and exit code
  and signal classification. `server/src/routes/studio/tests.rs`: every status of the
  endpoint through the real router with an unreachable database (200 for PNG, JPEG and
  SVG; 422 codes; 413; 408 for a stalled or trickling body, and a body in steady chunks
  getting through; 503 with the upload slots held, without reading the body; 503 with
  the processing slot held; 429 from the bucket, without reading the body; Flip
  clearing `faces_left`; Flip and Fit on the SVG the page rebuilds from a saved path
  matching the original file's fix; the slot held while processing and after an
  in-process timeout; crashes, timeouts and bugs mapped), the token bucket's clock, and
  the page (every placeholder carrying the default head or tail, every reference with a
  clean `data-d`, the browser's format check giving the server's answer for every
  rejected format, and studio.js building the design kit's SVG file).
- Fixtures live in `server/tests/fixtures/design_kit/`: under `catalog/`, all 184
  official SVGs, the investigation's `metrics_summary.csv` (the corpus oracle) and
  `metrics_detail.csv` (centroid, holes and bounds for the 12 samples the metric oracle
  checks); the raw reference SVGs under `refs/`; an Illustrator Save As export under
  `illustrator/`; under `template/`, the guide overlays
  and the SVG template (`head-template.svg`), all copies of the template generator's
  output; and a PIL progressive JPEG under `jpeg/`. Reference-ghost overlays are
  rendered in the tests.

```
cargo test -p arena --test design_kit_raster --test design_kit_metrics --test design_kit_svg \
  --test design_kit_catalog --test design_kit_refs
cargo test -p arena --lib design_kit
cargo test -p arena --bin arena design_kit_refs
cargo test -p arena --test studio_worker
cargo test -p arena --lib studio_worker
cargo test -p arena --bin arena studio
```
