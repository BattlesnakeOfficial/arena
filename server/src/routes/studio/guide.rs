//! `GET /customizations/studio/guide`: how to draw a head or tail, with the templates to
//! download.
//!
//! The studio's checks link here (`Lint::guide_anchor`), so the ids of the rules
//! (`#colour`, `#holes`, `#neck`, `#direction`, `#small`, `#fill`, `#margins`, `#guides`)
//! are part of the page's contract, along with `#templates`, `#first-head` and
//! `#procreate`. The illustrations are server-rendered from the reference shapes with
//! [`snake_board`]; the page needs no JavaScript.

use arena::design_kit::{AssetKind, refs::RefShape};
use axum::response::IntoResponse;
use maud::{Markup, html};

use super::page::default_ref;
use crate::{
    components::{
        page_factory::PageFactory,
        snake_board::{ShapeRef, four_directions_board, snake_board},
    },
    static_assets::asset_url,
};

/// Where the studio is, for links back to it.
pub const STUDIO_PATH: &str = "/customizations/studio";
/// This page.
pub const GUIDE_PATH: &str = "/customizations/studio/guide";

/// The anchors the studio's checks link to, in page order, each with what its section is
/// about: the end of a check's "Learn more about …" link. The studio page hands these to
/// its script (`data-guide-topics`), so this is the only list.
pub const RULE_TOPICS: [(&str, &str); 8] = [
    ("colour", "using one colour"),
    ("holes", "holes"),
    ("neck", "the neck"),
    ("direction", "which way to face"),
    ("small", "small details"),
    ("fill", "filling your shape"),
    ("margins", "drawing edge to edge"),
    ("guides", "hiding the guides"),
];

/// The anchors of [`RULE_TOPICS`].
pub const RULE_ANCHORS: [&str; 8] = {
    let mut anchors = [""; 8];
    let mut i = 0;
    while i < anchors.len() {
        anchors[i] = RULE_TOPICS[i].0;
        i += 1;
    }
    anchors
};

/// One downloadable file in `static/design-kit/`.
#[derive(Debug, Clone, Copy)]
pub struct Download {
    /// File name, also the `download` attribute.
    pub file: &'static str,
    /// Button label: the apps it is for.
    pub label: &'static str,
    /// What it is, after the kind in its accessible name ("…, head template").
    pub what: &'static str,
}

impl Download {
    pub fn href(&self) -> String {
        asset_url(&format!("design-kit/{}", self.file))
    }
}

/// The two templates for a kind: the layered PSD (Procreate, and Illustrator on iPad)
/// and the layered SVG (vector apps).
pub const fn templates(kind: AssetKind) -> [Download; 2] {
    match kind {
        AssetKind::Head => [
            Download {
                file: "battlesnake-head-template.psd",
                label: "Procreate (PSD)",
                what: "template",
            },
            Download {
                file: "battlesnake-head-template.svg",
                label: "Illustrator · Inkscape · Affinity (SVG)",
                what: "template",
            },
        ],
        AssetKind::Tail => [
            Download {
                file: "battlesnake-tail-template.psd",
                label: "Procreate (PSD)",
                what: "template",
            },
            Download {
                file: "battlesnake-tail-template.svg",
                label: "Illustrator · Inkscape · Affinity (SVG)",
                what: "template",
            },
        ],
    }
}

/// The guides alone, for apps that open neither template.
pub const fn guides_png(kind: AssetKind) -> Download {
    match kind {
        AssetKind::Head => Download {
            file: "battlesnake-head-guide.png",
            label: "Guides only (PNG)",
            what: "guides",
        },
        AssetKind::Tail => Download {
            file: "battlesnake-tail-guide.png",
            label: "Guides only (PNG)",
            what: "guides",
        },
    }
}

/// The finished drawing behind the studio's "Try an example".
pub const EXAMPLE_DRAWING: &str = "design-kit/example-drawing.png";

fn kind_word(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Head => "head",
        AssetKind::Tail => "tail",
    }
}

/// A download link. Its accessible name starts with the visible label and adds which
/// kind it is for ("Procreate (PSD), head template").
pub(crate) fn download_button(d: Download, kind: AssetKind, class: &str) -> Markup {
    html! {
        a class=(class) href=(d.href()) download=(d.file)
            aria-label={ (d.label) ", " (kind_word(kind)) " " (d.what) } {
            (d.label)
        }
    }
}

/// GET /customizations/studio/guide
pub async fn guide_page(page_factory: PageFactory) -> impl IntoResponse {
    page_factory
        .create_page(
            "Make your own head & tail".to_string(),
            Box::new(guide_markup()),
        )
        .with_description(
            "How to draw a Battlesnake head or tail: templates for Procreate and vector \
             apps, the rules that make a head read at game size, and your first head in 10 \
             minutes.",
        )
}

pub(crate) fn guide_markup() -> Markup {
    html! {
        div .guide data-testid="studio-guide" {
            div class="page-head" {
                h1 { "Make your own head & tail" }
                div class="sub" {
                    "Every snake in the arena has a head and a tail, and you can design your "
                    "own with a stylus or a vector app."
                }
            }
            p class="guide-lede" {
                "This page covers how the board draws your art, what makes a head read well, "
                "and how to get from a blank template to a preview in the "
                a href=(STUDIO_PATH) { "Head & Tail Studio" } ". "
                "In a hurry? Skip to " a href="#first-head" { "your first head in 10 minutes" } "."
            }
            nav class="guide-toc" aria-label="On this page" {
                ul {
                    li { a href="#templates" { "Templates" } }
                    li { a href="#rules" { "The rules" } }
                    li { a href="#anatomy" { "Anatomy" } }
                    li { a href="#first-head" { "First head in 10 minutes" } }
                    li { a href="#procreate" { "Procreate" } }
                    li { a href="#vector" { "Vector apps" } }
                    li { a href="#next" { "Next steps" } }
                }
            }

            (templates_section())
            (rules_section())
            (anatomy_section())
            (first_head_section())
            (procreate_section())
            (vector_section())

            section #preview class="guide-section" aria-labelledby="guide-preview" {
                h2 #guide-preview { "Preview it in the studio" }
                p {
                    "Upload your PNG, JPEG or SVG to the Head & Tail Studio. You'll see your "
                    "design on a board straight away:"
                }
                ul {
                    li { "in any snake colour, facing all four directions" }
                    li { "at real game size and close up" }
                    li { "on light and dark boards" }
                    li { "paired with your own tail (or head) or an official one" }
                }
                p {
                    "The studio flags common problems, like a gap at the neck, a head facing "
                    "the wrong way or leftover guides, and offers one-tap " strong { "Flip" }
                    " and " strong { "Fit" } " fixes. When you're happy, download the cleaned "
                    "100 × 100 SVG: that file is exactly what a Battlesnake head or tail is "
                    "made of."
                }
                p { a class="btn solid" href=(STUDIO_PATH) { "Open the Head & Tail Studio" } }
            }

            section #next class="guide-section" aria-labelledby="guide-next" {
                h2 #guide-next { "Next steps" }
                p {
                    "Save the preview image and post it with your SVG in the "
                    a href="/discord" { "Battlesnake Discord" }
                    ", to show it off or to ask about getting it into the game."
                }
            }
        }
    }
}

fn templates_section() -> Markup {
    html! {
        section #templates class="guide-section" aria-labelledby="guide-templates" {
            h2 #guide-templates { "Get a template" }
            p {
                "Each template is a 1000 × 1000 px square with the guides on their own layer, a "
                "white background, an empty " strong { "Draw here" } " layer, and official "
                "shapes as optional light references to trace over."
            }
            div class="guide-downloads" {
                @for kind in [AssetKind::Head, AssetKind::Tail] {
                    div class="guide-download" {
                        h3 { @if kind == AssetKind::Head { "Head" } @else { "Tail" } }
                        div class="guide-download-buttons" {
                            @for (i, d) in templates(kind).into_iter().enumerate() {
                                (download_button(d, kind, if i == 0 { "btn solid" } else { "btn" }))
                            }
                        }
                        p class="guide-muted" {
                            "Other app? "
                            (download_button(guides_png(kind), kind, "guide-minor-download"))
                            ": put it on a layer above your drawing, set that layer to "
                            strong { "Multiply" } ", and hide it before you export."
                        }
                    }
                }
            }
        }
    }
}

/// One rule of the medium: an anchor the studio's checks link to.
fn rule(id: &str, title: &str, body: Markup) -> Markup {
    html! {
        div id=(id) class="guide-rule" {
            h3 { (title) }
            (body)
        }
    }
}

fn rules_section() -> Markup {
    let head = shape_ref(default_ref(AssetKind::Head));
    let tail = shape_ref(default_ref(AssetKind::Tail));
    html! {
        section #rules class="guide-section" aria-labelledby="guide-rules" {
            h2 #guide-rules { "The rules of the medium" }
            (rule("colour", "It's one colour", html! {
                p {
                    "The board paints your whole drawing in the snake's colour, so it doesn't "
                    "matter which colour you draw in. " strong { "Use one dark fill." }
                    " Black is easiest. If you use several colours, the studio flattens them: "
                    "dark areas become the snake and light areas become holes. Soft or "
                    "see-through strokes only count where they're at least half opaque."
                }
                p {
                    "In an SVG only the filled shapes count. Clipping paths are applied, but "
                    "masks are ignored (masked shapes show in full), and filters such as blurs "
                    "and drop shadows, scripts and links are left out, so check the preview."
                }
            }))
            (rule("holes", "Details are holes", html! {
                p {
                    strong { "Anything you fill white becomes a hole" }
                    ", and so does anything you leave empty. Eyes, mouths, stripes and freckles "
                    "are cut-outs that show the board through, like a rubber stamp. On the dark "
                    "board your holes look dark, so make sure the shape still reads that way."
                }
            }))
            (rule("neck", "The neck is the whole left edge", html! {
                p {
                    "The body joins your head along the left edge, top to bottom. Fill that "
                    "edge completely or you'll see a notch where they meet. The pink band on the "
                    "template marks it. Tails work the same way: the body joins on the left."
                }
            }))
            (rule("direction", "Face right", html! {
                p {
                    "Draw the head looking right and the tail tip pointing right. The game "
                    "turns your art when the snake goes up or down, and mirrors it when the "
                    "snake heads left. Because of the mirroring, words and logos read backwards "
                    "half the time, so skip lettering."
                }
                figure class="guide-figure guide-board" {
                    (snake_board(&four_directions_board(
                        head,
                        tail,
                        "guide-directions light",
                        "Four pink snakes wearing the default head and tail, facing right, left, up and down",
                    )))
                    figcaption { "One drawing, four directions: the game turns and mirrors it for you." }
                }
            }))
            (rule("small", "It's tiny in a game", html! {
                p {
                    "Your 1000 px square is shown about " strong { "20 px wide on a phone" }
                    ", about 40 px on an iPad and about 50 px on a desktop. That's a 50× shrink "
                    "on a phone. Go bold: use big eyes and chunky shapes, and "
                    strong { "keep every detail and every gap at least 40 px" }
                    " (under half a grid square; the swatches in the template's corner show "
                    "the size). Zoom way out and squint. If you can't tell what it is, simplify. "
                    "The studio's " strong { "Game size" } " view shows you the real thing."
                }
                p {
                    "Export at the template's full " strong { "1000 × 1000 px" }
                    ". A smaller image is scaled up and its edges look soft."
                }
            }))
            (rule("fill", "Fill your shape", html! {
                p {
                    "Draw solid shapes, not outlines. A line drawing of a head becomes a thin "
                    "ring on the board. Fill the inside, then erase the details you want as "
                    "holes. In vector apps, strokes and overlapping shapes are fine: "
                    "converting strokes or merging shapes is optional, because the studio does "
                    "it for you."
                }
                p {
                    "Live text and placed images are left out. Turn text into shapes first ("
                    strong { "Type → Create Outlines" } " in Illustrator, "
                    strong { "Path → Object to Path" } " in Inkscape), and draw with shapes "
                    "rather than placing a picture, or upload the picture as a PNG."
                }
            }))
            (rule("margins", "Draw edge to edge", html! {
                p {
                    "Let your drawing touch the left, top and bottom edges of the square. A small "
                    "drawing floating in the middle looks tiny in a game and leaves a gap at the "
                    "neck. If you've already drawn it small, the studio's " strong { "Fit" }
                    " button stretches it for you."
                }
                p {
                    "Keep the canvas square, like the 1000 × 1000 px template: a canvas of any "
                    "other shape is centred in a square. Anything outside the square is cut off."
                }
            }))
            (rule("guides", "Hide the guides", html! {
                p {
                    "Before you export, hide the " strong { "Guides" } " layer and any "
                    strong { "Reference" } " layer, and " strong { "leave Background on" }
                    ". If you forget, the studio ignores the template's colours and tells you, "
                    "but hiding them gives the cleanest result."
                }
                p {
                    "In an SVG with a " strong { "draw-here" } " layer, only that layer is used: "
                    "shapes on your other layers are left out, so move everything you drew into it."
                }
            }))
        }
    }
}

fn shape_ref(r: &'static RefShape) -> ShapeRef<'static> {
    ShapeRef {
        d: r.d,
        fill_rule: r.fill_rule,
        class: None,
    }
}

/// A numbered marker on an anatomy close-up.
fn marker(n: u8, x: f32, y: f32) -> Markup {
    html! {
        g class="guide-marker" transform={ "translate(" (x) " " (y) ")" } {
            circle r="6" {}
            text y="2.6" { (n) }
        }
    }
}

/// The asset at large size, framed like one board cell, with a body stub where the body
/// joins and numbered markers on its features.
fn closeup(r: &RefShape, label: &str, markers: Markup) -> Markup {
    html! {
        svg class="guide-closeup" xmlns="http://www.w3.org/2000/svg" viewBox="-20 -8 128 116"
            role="img" aria-label=(label) {
            rect class="guide-closeup-bg" width="100" height="100" {}
            rect class="guide-closeup-body" x="-20" width="20" height="100" {}
            path class="guide-closeup-shape" d=(r.d) fill-rule=(r.fill_rule.as_svg()) {}
            rect class="guide-closeup-frame" width="100" height="100" fill="none" {}
            (markers)
        }
    }
}

fn anatomy_section() -> Markup {
    let head = default_ref(AssetKind::Head);
    let tail = default_ref(AssetKind::Tail);
    html! {
        section #anatomy class="guide-section" aria-labelledby="guide-anatomy" {
            h2 #guide-anatomy { "Anatomy of a head and a tail" }
            div class="guide-anatomy" {
                figure class="guide-figure" {
                    (closeup(head, "The default head, with its eye, mouth and neck marked", html! {
                        (marker(1, 31.0, 28.5))
                        (marker(2, 86.0, 55.0))
                        (marker(3, -10.0, 72.0))
                    }))
                    figcaption {
                        ol class="guide-legend" {
                            li { strong { "Eye:" } " a hole, in the upper-left third." }
                            li { strong { "Mouth:" } " a notch cut into the right edge." }
                            li { strong { "Neck:" } " the full left edge, where the body joins." }
                        }
                    }
                }
                figure class="guide-figure" {
                    (closeup(tail, "The default tail, with where the body joins and its tip marked", html! {
                        (marker(1, -10.0, 72.0))
                        (marker(2, 86.0, 50.0))
                    }))
                    figcaption {
                        ol class="guide-legend" {
                            li { strong { "Body:" } " joins along the full left edge." }
                            li { strong { "Tip:" } " tails taper to the right." }
                        }
                    }
                }
            }
            p { "These patterns come from the official heads and tails, measured on the 1000 px template:" }
            ul {
                li {
                    strong { "Full-height left edge." }
                    " Every official head and tail fills almost all of its left edge, and most "
                    "fill every pixel of it."
                }
                li {
                    strong { "An eye cutout in the upper-left third." }
                    " The eye is usually a round hole about 185 px across (just under two grid "
                    "squares, about 18% of the height). Most official heads put it a third of "
                    "the way in from the left and a little above the middle (around x 360, "
                    "y 390); the default head above keeps it close to the neck."
                }
                li {
                    strong { "A mouth notch in the right edge." }
                    " Usually a wedge or a smile cut into the snout, or a slot in the lower jaw. "
                    "No official head fills more than half of its right edge."
                }
                li {
                    strong { "A full face, then a snout." }
                    " Heads stay about three-quarters of the square's height until about x 600, "
                    "then narrow to the snout. A typical head covers about two-thirds of the square."
                }
                li {
                    strong { "Tails taper to the tip." }
                    " By the middle a tail is about half the height, narrowing to a point, a "
                    "round cap, a curl or a rattle at the right. A typical tail covers about half "
                    "the square."
                }
            }
        }
    }
}

fn first_head_section() -> Markup {
    html! {
        section #first-head class="guide-section" aria-labelledby="guide-first-head" {
            h2 #guide-first-head { "Your first head in 10 minutes" }
            p { "This uses Procreate; the steps are the same in any app with layers." }
            ol class="guide-steps" {
                li {
                    strong { "Get the template (1 min)." }
                    " Tap " strong { "Procreate (PSD)" } " for the head, above or in the studio. "
                    "In Procreate's Gallery tap " strong { "Import" } " and pick "
                    code { "battlesnake-head-template.psd" } " from Files → Downloads."
                }
                li {
                    strong { "Turn on the ghost (30 s)." }
                    " Open Layers and tick " strong { "Reference: default head" }
                    ". A light grey head appears to trace over."
                }
                li {
                    strong { "Block in a silhouette (3 min)." }
                    " Select " strong { "Draw here (black)" } " and pick "
                    strong { "Inking → Studio Pen" } " in black. Start at the top-left corner, run "
                    "along the top, round the snout however you like, and come back along the "
                    "bottom to the bottom-left corner. Then drag the colour dot into the shape to "
                    "fill it. Bigger and rounder than the ghost is fine."
                }
                li {
                    strong { "Erase an eye (1 min)." }
                    " Switch to the " strong { "Eraser" } " with the same Studio Pen and erase a "
                    "round eye in the upper-left third, about two grid squares across. If you "
                    "like, draw a black pupil back in, leaving a white ring at least 40 px wide."
                }
                li {
                    strong { "Change the mouth (2 min)." }
                    " Erase a notch or a smile into the right edge, or draw a fang back in. Keep "
                    "gaps at least 40 px."
                }
                li {
                    strong { "Hide the ghost (30 s)." }
                    " Untick the " strong { "Reference" } " layer and the " strong { "Guides" }
                    " layer. Leave " strong { "Background" } " on."
                }
                li {
                    strong { "Export and upload (2 min)." }
                    " Tap " strong { "Actions (wrench) → Share → PNG → Save to Files" }
                    ". In the studio, choose " strong { "Head" } " and upload the file."
                }
            }
            p {
                "Want to see what a finished drawing looks like first? Tap "
                strong { "Try an example" } " in the " a href=(STUDIO_PATH) { "studio" } "."
            }
        }
    }
}

fn procreate_section() -> Markup {
    html! {
        section #procreate class="guide-section" aria-labelledby="guide-procreate" {
            h2 #guide-procreate { "Procreate" }
            ol class="guide-steps" {
                li {
                    "Tap " strong { "Procreate (PSD)" } " for the head or the tail. Safari saves "
                    "it to " strong { "Files → Downloads" } "."
                }
                li {
                    "In the Procreate " strong { "Gallery" } ", tap " strong { "Import" }
                    " and choose the file. Don't use " em { "Insert a file" }
                    " from inside a canvas: it flattens the layers."
                }
                li {
                    "Select the " strong { "Draw here (black)" } " layer. Draw in black with a "
                    "solid, hard-edged brush such as " strong { "Inking → Studio Pen" } " or "
                    strong { "Calligraphy → Monoline" }
                    ". Fill big areas by dragging the colour dot into a closed shape."
                }
                li {
                    strong { "Erase for holes." }
                    " Use the Eraser with the same brush for eyes and details."
                }
                li {
                    "Want to see how an official head is shaped? Tick a " strong { "Reference" }
                    " layer for a light ghost."
                }
                li {
                    strong { "Hide Guides and Reference; leave Background on." }
                }
                li {
                    "Tap " strong { "Actions (wrench) → Share → PNG → Save to Files" }
                    ", then upload it in the studio. Uploading from Photos works too."
                }
            }
        }
    }
}

fn vector_section() -> Markup {
    html! {
        section #vector class="guide-section" aria-labelledby="guide-vector" {
            h2 #guide-vector { "Illustrator, Inkscape, Affinity or Figma" }
            ol class="guide-steps" {
                li {
                    "Tap " strong { "Illustrator · Inkscape · Affinity (SVG)" } " and open the "
                    "file. Inkscape and Affinity show its layers by name. Illustrator puts them "
                    "as groups inside \u{201c}Layer 1\u{201d}, so lock the " em { "guides" }
                    " group. Figma imports plain groups, which is fine."
                }
                li {
                    "Draw inside " strong { "draw-here" } " using " strong { "one dark fill" }
                    ". " strong { "Light fills become holes." }
                    " Strokes and overlapping shapes are fine; the studio converts and merges them."
                }
                li {
                    "Skip gradients, placed images and live text. The studio flattens or "
                    "ignores them."
                }
                li {
                    "Hide the guides and reference layers. " strong { "Export SVG" }
                    ", or a 1000 × 1000 " strong { "PNG" } " with a white background."
                }
            }
            p {
                "On Illustrator for iPad you can open the PSD template instead and draw on a "
                "new layer above the guides."
            }
        }
    }
}
