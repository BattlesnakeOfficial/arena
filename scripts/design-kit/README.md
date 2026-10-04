# Head & tail design kit generator

`generate.py` builds the downloadable templates for the Head & Tail Studio
(`/customizations/studio`, with its guide at `/customizations/studio/guide`) and
writes them to `server/static/design-kit/`:

| File | What it is |
|---|---|
| `battlesnake-head-template.psd`, `battlesnake-tail-template.psd` | 1000×1000 layered PSD for Procreate (Gallery → Import) and Illustrator on iPad |
| `battlesnake-head-template.svg`, `battlesnake-tail-template.svg` | `viewBox="0 0 100 100"`, Inkscape-style layers, for vector apps |
| `battlesnake-head-guide.png`, `battlesnake-tail-guide.png` | The guides layer alone, transparent, 1000×1000 (for apps that can't open either template; the design_kit tests composite them with drawings) |
| `example-drawing.png` | A hand-drawn-looking head used by the studio's "Try an example" button |

Nothing in the server runs this script. It's Python only because psd-tools is
the one maintained library that writes layered PSDs.

## Regenerating

You need Python 3.12 (tested with 3.12.12) and the cairo C library, which
CairoSVG loads at runtime (`apt install libcairo2` on Debian/Ubuntu,
`brew install cairo` on macOS).

```bash
cd scripts/design-kit
python3.12 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python generate.py              # writes ../../server/static/design-kit/
.venv/bin/python generate.py --previews   # also writes previews/ (not committed)
git status ../../server/static/design-kit # review the diff, then commit
```

The design_kit tests read the committed outputs directly, so after a
regeneration run them (see the template contract below) before committing.

`--out DIR` writes somewhere else. If the script isn't inside the arena repo,
it defaults to `./out`.

The output is deterministic: two runs with the same inputs give byte-identical
files, so regenerating without changes leaves `git status` clean. The reference
run used Ubuntu 24.04 with cairo 1.18.0. A different cairo version can shift
anti-aliased pixels in the PSDs and guide PNGs. That's harmless, but it shows up
as a binary diff, so only commit a regeneration when you meant to change
something.

On every run the script also checks its own output and exits non-zero if:
- the SVG layer ids or colours break the contract below
- a PSD layer has a mask, or "Draw here" isn't fully transparent
- `example-drawing.png` would trip any of the studio's head lints (coverage,
  left edge, margins, direction, outline-only)

It prints each PSD's size and layer list (name, visibility, blend mode, locks,
alpha range) so you can eyeball it.

## Template contract

The studio's ink rule (`server/src/design_kit/palette.rs` and the SVG filter)
relies on these, so that a file uploaded with the guides still showing works.
`server/tests/design_kit_templates.rs` parses the committed SVG templates and
fails if they drift from `design_kit::palette`. **If you change anything here,
change `palette.rs` in the same PR**, and run the design_kit tests: the ink
matrix and `dark_saturated_inks_survive_the_template` (in
`server/tests/design_kit_raster.rs`) composite the guide PNGs with drawings.

**Palette.** Every fill and stroke in the `guides` and `reference-*` layers is
one of these six colours, and nothing else (no white halos, no black):

| Colour | Luma | Contrast on white | Used for |
|---|---|---|---|
| `#bfe3f7` | 0.87 | 1.35:1 | grid lines, vertical centre line |
| `#9eb0f4` | 0.69 | 2.11:1 | canvas border, the "middle" spine line |
| `#7e8fb8` | 0.56 | 3.23:1 | every label but the neck's, the arrow, detail-size swatches |
| `#ff94b8` | 0.68 | 2.06:1 | neck/attach edge: band, strip and arrows |
| `#b4808c` | 0.55 | 3.28:1 | the vertical "neck attaches here" label |
| `#c8c2d4` | 0.77 | 1.73:1 | reference ghost (baked into the pixels, not layer opacity) |

Why these: the studio treats anything within RGB distance 48 of a template
colour as "template, not ink" (and, when the reference shows, anything within
32 of a guide colour multiplied over the ghost). Every colour here is lighter than 50% luma, so
that zone stays clear of the dark colours people draw with (navy, sapphire,
royal blue, steel blue, crimson, raspberry, teal). The first palette used dark
blues and a crimson for its labels, and drawings in those colours came back
empty. The text colours are the darkest the rule allows, for about 3.2:1
contrast on white (WCAG's bar for large text), and the labels are large and
bold to make up for the rest. They are all saturated hues, so the studio can
tell them from the grey anti-aliasing of black ink.

**SVG layer ids.** These are top-level `<g inkscape:groupmode="layer">`:

| id | Contents |
|---|---|
| `draw-here` | Empty; `fill="#000000"`; the starting layer |
| `guides` | Locked; `mix-blend-mode:multiply` |
| `reference-<slug>` | Hidden and locked: `reference-default` and `reference-smile` (head), `reference-default` and `reference-round-bum` (tail) |

The studio drops `guides`, `reference-*` and `display:none` subtrees. If
`draw-here` exists it uses only that. It also drops any element whose fill or
stroke is a palette colour, because Figma strips ids on import.

**PSD layers**, top to bottom:

| Layer | Settings |
|---|---|
| `Guides (hide before export)` | Multiply, locked |
| `Draw here (black)` | Normal, unlocked |
| `Reference: … (optional)` | Hidden, locked |
| `Background (leave on)` | White, locked |

The guides use Multiply so that an export made with the guides still on keeps
every ink pixel black.

The document is RGBA, so each layer's shape is its own transparency channel
and no layer has a mask. (In an RGB document psd-tools stores a layer's alpha
as a mask over solid pixels: "Draw here" came out solid black under a hide-all
mask, so paint on it stayed hidden in apps that keep masks, and the canvas
opened black in apps that drop them.) "Draw here" is fully transparent. The
merged composite's fourth channel is its transparency (a negative layer
count), as Photoshop writes a layered document. The script checks this on
every run, and `server/tests/design_kit_templates.rs` parses the committed
PSDs and checks it too.

**Copy.** The guides say "keep details ≥ 40 px", and the studio's lint
messages and the guide page (`server/src/routes/studio/guide.rs`) use the same
number. Change all three together.

## Why the outputs are committed

- `server/static/` is compiled into the server binary (`include_dir!`) and
  served with a content hash. The Rust build, CI and the Docker image therefore
  need the files to exist, and none of them should need Python or cairo.
- The contract tests read the committed SVGs, PSDs and guide PNGs.
- Templates change rarely. A reviewer sees exactly which bytes users will
  download.

## Vendored inputs and licensing

- `vendor/fonts/DejaVuSans-Bold.ttf` (DejaVu 2.37) is used only to turn label
  text into outlines, so the templates contain paths, not fonts. It's under the
  Bitstream Vera licence plus public-domain DejaVu changes; the full text is in
  `vendor/fonts/LICENSE`. That licence allows redistribution as long as the
  font isn't sold by itself and modified copies are renamed.
- `vendor/heads/{default,smile}.svg` and `vendor/tails/{default,round-bum}.svg`
  are Battlesnake's own catalog art, fetched unmodified from
  `media.battlesnake.com/snakes/{heads,tails}/<slug>.svg`. All are in the free
  Standard group. They're included as drawing references in Battlesnake's own
  repository. They aren't covered by the repo's code licence, so don't reuse
  them elsewhere. Only their filled geometry is re-emitted (as the light
  `#c8c2d4` ghost).
- `example-drawing.png` is original, drawn procedurally by
  `example_drawing.py` (seeded, so it's deterministic). It doesn't copy any
  catalog head, and it follows the guide's own rule: every hole, and every ring
  of ink or paper between holes, is at least 40 px.
- Python dependencies (pinned in `requirements.txt`) run only at generation
  time and aren't shipped: psd-tools (MIT), fontTools (MIT), Pillow (MIT-CMU),
  numpy (BSD-3-Clause), CairoSVG (LGPL-3.0).
