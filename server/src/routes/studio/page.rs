//! `GET /customizations/studio`: the Head & Tail Studio page.
//!
//! Everything is server-rendered: every preview board (with the default head and tail
//! in place), a card per slot ("Your snake": the head and the tail, each with its
//! upload button, its catalog styles and a thumbnail), a close-up per slot, and empty
//! check lists per slot. `static/studio.js` posts uploads to the processing endpoint
//! and then only sets attributes and text: `d`/`fill-rule` on the placeholder paths,
//! the `--studio-snake` colour variable, the board theme class, and the checks.
//!
//! Placeholder paths: every board's head is `path.studio-head` and every tail
//! `path.studio-tail`, whatever fills that slot; each slot's close-up is
//! `path#studio-closeup-path-{head,tail}` and its thumbnail `path#studio-thumb-path-…`.
//! Every per-slot id is `studio-<part>-<kind>` ([`slot_id`]).

use arena::design_kit::{
    AssetKind, FillRule, ProcessError, SIGNATURES, Sniffed,
    refs::{REFS, RefShape},
};
use axum::response::IntoResponse;
use maud::{Markup, html};
use serde::Serialize;

use super::guide::{
    EXAMPLE_DRAWING, GUIDE_PATH, RULE_TOPICS, download_button, kind_word, templates,
};
use super::process::MAX_BODY_BYTES;
use crate::{
    components::{
        page_factory::PageFactory,
        snake_board::{ShapeRef, four_directions_board, live_loop_board, snake_board},
    },
    static_assets::asset_url,
};

/// Colour presets: name, colour, and a hint shown under the swatches when picked.
pub const COLOR_PRESETS: [(&str, &str, Option<&str>); 9] = [
    ("Pink", "#ff4f86", None),
    ("Red", "#e5383b", None),
    ("Orange", "#ff8c1a", None),
    ("Yellow", "#f7c948", None),
    ("Green", "#2bb673", None),
    ("Blue", "#3a86ff", None),
    ("Purple", "#8f5cf7", None),
    (
        "Grey",
        "#888888",
        Some("Grey is the game's default snake colour."),
    ),
    (
        "Charcoal",
        "#3d3d3d",
        Some(
            "Charcoal is close to the dark board's #393939, so check your design still stands out on it.",
        ),
    ),
];

/// The default colour (the first preset).
pub const DEFAULT_COLOR: &str = COLOR_PRESETS[0].1;

/// The page's preview views, in segmented-control order: (value, label).
const VIEWS: [(&str, &str); 4] = [
    ("closeup", "Close-up"),
    ("live", "Live"),
    ("all", "All directions"),
    ("game", "Game size"),
];

fn refs(kind: AssetKind) -> impl Iterator<Item = &'static RefShape> {
    REFS.iter().filter(move |r| r.kind == kind)
}

/// Only if the reference table lost its defaults (a test guards against that).
const FALLBACK_REF: RefShape = RefShape {
    slug: "default",
    kind: AssetKind::Head,
    display_name: "Default",
    d: "M0 0L100 0L100 100L0 100Z",
    fill_rule: FillRule::NonZero,
};

/// The `default` reference of a kind: what the slots show before any upload.
pub(crate) fn default_ref(kind: AssetKind) -> &'static RefShape {
    refs(kind)
        .find(|r| r.slug == "default")
        .or_else(|| refs(kind).next())
        .unwrap_or(&FALLBACK_REF)
}

/// The browser's instant check before an upload, rendered from the server's own rules
/// (`data-sniff` on `#studio-slots`): the formats [`design_kit::sniff`] rejects, with the
/// same advice, and the size limit. Anything else is posted and the server decides.
///
/// [`design_kit::sniff`]: arena::design_kit::sniff
#[derive(Debug, Serialize)]
pub(crate) struct ClientSniff {
    pub max_bytes: usize,
    pub too_large: String,
    pub rejected: Vec<ClientSignature>,
}

/// One rejected format: every `[offset, alternatives]` part must match (one of the
/// alternatives starts at `offset`), as [`arena::design_kit::Signature::matches`].
#[derive(Debug, Serialize)]
pub(crate) struct ClientSignature {
    pub at: Vec<(usize, &'static [&'static [u8]])>,
    pub message: String,
}

pub(crate) fn client_sniff() -> ClientSniff {
    let rejected = SIGNATURES
        .iter()
        .filter_map(|sig| match sig.sniffed {
            Sniffed::Rejected(format) => Some(ClientSignature {
                at: sig.parts.to_vec(),
                message: ProcessError::UnsupportedFormat(format).user_message(),
            }),
            Sniffed::Accepted(_) => None,
        })
        .collect();
    let too_large = ProcessError::TooLarge {
        bytes: MAX_BODY_BYTES + 1,
        max: MAX_BODY_BYTES,
    };
    ClientSniff {
        max_bytes: MAX_BODY_BYTES,
        too_large: too_large.user_message(),
        rejected,
    }
}

fn placeholder(shape: &'static RefShape, class: &'static str) -> ShapeRef<'static> {
    ShapeRef {
        d: shape.d,
        fill_rule: shape.fill_rule,
        class: Some(class),
    }
}

/// GET /customizations/studio
pub async fn studio_page(page_factory: PageFactory) -> impl IntoResponse {
    page_factory
        .create_page("Head & Tail Studio".to_string(), Box::new(studio_markup()))
        .with_description(
            "Upload a Battlesnake head or tail you drew and preview it on a real board: \
             any colour, every direction, at game size and close up.",
        )
}

pub(crate) fn studio_markup() -> Markup {
    let head = placeholder(default_ref(AssetKind::Head), "studio-head");
    let tail = placeholder(default_ref(AssetKind::Tail), "studio-tail");
    let label_all = "Four pink snakes wearing the default head and the default tail, facing \
                     right, left, up and down";
    // Without the check the browser just posts every file; the server answers anyway.
    let sniff = serde_json::to_string(&client_sniff()).unwrap_or_default();
    // The guide's sections the checks link to, for "Learn more about …".
    let topics = serde_json::to_string(&RULE_TOPICS).unwrap_or_default();
    html! {
        div #studio .studio data-testid="studio" data-guide=(GUIDE_PATH)
            data-guide-topics=(topics) {
            div class="page-head" {
                h1 { "Head & Tail Studio" }
                div class="sub" {
                    "Upload a head and a tail you drew and see them together on a real "
                    "Battlesnake board: any colour, every direction, at game size and close up."
                }
            }
            noscript {
                p class="studio-noscript" { "The studio needs JavaScript to process your drawing." }
            }

            // "Your snake" first, so both upload buttons are near the top on a first visit
            // too (an iPad in landscape included); "Start here" follows, open until then.
            div class="studio-layout" {
                section #studio-snake class="studio-panel studio-snake" data-testid="studio-snake"
                    aria-labelledby="studio-snake-heading" {
                    h2 #studio-snake-heading { "Your snake" }
                    p class="studio-muted studio-snake-lede" {
                        "A head and a tail, together on every board. Upload either, or both."
                    }
                    div #studio-slots class="studio-slots" data-sniff=(sniff) {
                        (slot_card(AssetKind::Head))
                        (slot_card(AssetKind::Tail))
                    }
                    p class="studio-muted studio-slots-hint" {
                        "PNG, JPEG or SVG. You can also drop a file on a card."
                    }
                    p #studio-summary class="studio-summary" data-testid="studio-summary" hidden {}
                    p #studio-status class="studio-status" role="status" aria-live="polite"
                        data-testid="studio-status" { "Try it with your own drawing." }
                    div #studio-top-warning class="studio-top-warning" data-testid="studio-top-warning" hidden {
                        span class="studio-lint-icon" aria-hidden="true" { "!" }
                        p #studio-top-warning-text {}
                        a #studio-top-learn class="studio-learn" hidden {}
                        button #studio-top-fix class="btn sm studio-fix" type="button"
                            data-action="fix" hidden {}
                    }
                }
                (start_here())

                section class="studio-panel studio-preview" aria-labelledby="studio-result-heading" {
                    h2 #studio-result-heading tabindex="-1" { "Preview" }
                    fieldset #studio-view class="studio-seg studio-views" {
                        legend class="vh" { "Preview view" }
                        div class="studio-seg-opts" {
                            @for (value, label) in VIEWS {
                                label class={ "studio-seg-opt studio-view-" (value) } {
                                    input type="radio" name="studio-view" value=(value) checked[value == "closeup"];
                                    span {
                                        (label)
                                        @if value == "closeup" {
                                            span class="studio-wide-only" { "\u{a0}& live" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div #studio-panes class="studio-panes" data-view="closeup" {
                        div class="studio-pane" data-pane="closeup" {
                            div #studio-closeup class="studio-closeups" {
                                (closeup(AssetKind::Head))
                                (closeup(AssetKind::Tail))
                            }
                            p class="studio-pane-note" {
                                "The body joins on the left edge. Red marks show gaps in the neck."
                            }
                        }
                        div class="studio-pane" data-pane="live" {
                            (snake_board(&live_loop_board(
                                head,
                                tail,
                                "studio-live light",
                                "A pink snake wearing the default head and the default tail, moving around the board",
                            )))
                            button #studio-play class="btn sm studio-play" type="button"
                                aria-label="Pause the live preview" { "Pause" }
                        }
                        div class="studio-pane" data-pane="all" {
                            (snake_board(&four_directions_board(head, tail, "studio-all light", label_all)))
                        }
                        div class="studio-pane" data-pane="game" {
                            p class="studio-pane-note" {
                                "Actual size: about how big a snake looks in a game on a phone and on an iPad."
                            }
                            figure class="studio-true-size phone" {
                                (snake_board(&four_directions_board(
                                    head,
                                    tail,
                                    "studio-game light",
                                    "Phone size: four pink snakes wearing the default head and the default tail",
                                )))
                                figcaption { "Phone (cells about 22 px)" }
                            }
                            figure class="studio-true-size tablet" {
                                (snake_board(&four_directions_board(
                                    head,
                                    tail,
                                    "studio-game light",
                                    "iPad size: four pink snakes wearing the default head and the default tail",
                                )))
                                figcaption { "iPad (cells about 40 px)" }
                            }
                        }
                    }
                }

                section #studio-lints class="studio-panel studio-lints" aria-labelledby="studio-lints-heading" {
                    h2 #studio-lints-heading { "Checks" }
                    p #studio-lints-empty class="studio-muted" {
                        "Upload a head or a tail to check it against the rules every official one follows."
                    }
                    (checks(AssetKind::Head))
                    (checks(AssetKind::Tail))
                }

                section class="studio-panel studio-controls" aria-labelledby="studio-controls-heading" {
                    h2 #studio-controls-heading { "Style" }
                    fieldset #studio-colors class="studio-colors" {
                        legend { "Snake colour" }
                        div class="studio-swatches" {
                            @for (name, hex, hint) in COLOR_PRESETS {
                                label class="studio-swatch" {
                                    input type="radio" name="studio-color" value=(hex)
                                        data-name=(name) data-hint=[hint] aria-label={ (name) " " (hex) }
                                        checked[hex == DEFAULT_COLOR];
                                    span class="studio-swatch-chip" style={ "background:" (hex) } {}
                                    span class="studio-swatch-name" aria-hidden="true" { (name) }
                                }
                            }
                            // The colour input covers the label like the radios do; the
                            // chip shows the custom colour once one is picked.
                            label #studio-swatch-custom class="studio-swatch studio-swatch-custom" {
                                input #studio-color-custom type="color" value=(DEFAULT_COLOR)
                                    aria-label="Custom colour";
                                span class="studio-swatch-chip studio-swatch-chip-custom" {}
                                span class="studio-swatch-name" aria-hidden="true" { "Custom" }
                            }
                        }
                        p #studio-color-hint class="studio-muted" hidden {}
                    }
                    fieldset #studio-theme class="studio-seg" {
                        legend { "Board" }
                        div class="studio-seg-opts" {
                            label class="studio-seg-opt" {
                                input type="radio" name="studio-theme" value="light" checked;
                                span { "Light" }
                            }
                            label class="studio-seg-opt" {
                                input type="radio" name="studio-theme" value="dark";
                                span { "Dark" }
                            }
                        }
                    }
                }

                section class="studio-panel studio-actions" aria-labelledby="studio-actions-heading" {
                    h2 #studio-actions-heading class="vh" { "Save" }
                    div class="studio-action-row" {
                        button #studio-save-image class="btn" type="button" { "Save preview image" }
                    }
                    p class="studio-privacy studio-muted" {
                        "Your files are processed and immediately discarded on our servers. "
                        "Your head, your tail and your settings are kept only in this browser."
                    }
                }
            }
        }
        script src=(asset_url("studio.js")) defer {}
    }
}

/// A per-slot element id: `studio-<part>-<kind>`, the same scheme studio.js uses.
pub(crate) fn slot_id(part: &str, kind: AssetKind) -> String {
    format!("studio-{part}-{}", kind_word(kind))
}

fn title_word(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Head => "Head",
        AssetKind::Tail => "Tail",
    }
}

/// One slot of "Your snake": a thumbnail and the name of what fills the slot, its
/// upload button (a label the file input covers, so it is the input for keyboard,
/// touch and VoiceOver; the whole card also takes a dropped file), the catalog styles
/// to use while the slot isn't the artist's own, and the upload's own actions, which
/// studio.js shows once there is one (and Undo, after Remove).
fn slot_card(kind: AssetKind) -> Markup {
    let k = kind_word(kind);
    let to = kind_word(match kind {
        AssetKind::Head => AssetKind::Tail,
        AssetKind::Tail => AssetKind::Head,
    });
    let shape = default_ref(kind);
    let id = |part: &str| slot_id(part, kind);
    html! {
        div id=(id("slot")) class="studio-slot" data-kind=(k) data-testid=(id("slot"))
            role="group" aria-labelledby=(id("title")) {
            svg id=(id("thumb")) class="studio-thumb" xmlns="http://www.w3.org/2000/svg"
                viewBox="-20 0 120 100" aria-hidden="true" focusable="false" {
                rect class="studio-thumb-bg" width="100" height="100" {}
                rect class="studio-thumb-body" x="-20" width="20" height="100" {}
                path id=(id("thumb-path")) class="studio-thumb-path" d=(shape.d)
                    fill-rule=(shape.fill_rule.as_svg()) {}
            }
            div class="studio-slot-who" {
                h3 id=(id("title")) class="studio-slot-title" { (title_word(kind)) }
                p id=(id("name")) class="studio-slot-name" { "Default " (k) }
            }
            label id=(id("upload")) class="btn solid studio-slot-upload" {
                input id=(id("file")) class="studio-file" type="file" name=(id("file"))
                    data-kind=(k)
                    accept=".png,.jpg,.jpeg,.svg,image/png,image/jpeg,image/svg+xml";
                span id=(id("upload-text")) { "Upload " (k) }
                span id=(id("upload-vh")) class="vh" {}
            }
            div class="field studio-style" {
                label for=(id("style")) { span class="vh" { (title_word(kind)) " " } "style" }
                select id=(id("style")) class="studio-style-select" data-kind=(k) {
                    @for r in refs(kind) {
                        option value=(r.slug) data-d=(r.d) data-fill-rule=(r.fill_rule.as_svg())
                            selected[r.slug == "default"] { (r.display_name) }
                    }
                }
            }
            div class="studio-slot-actions" {
                button id=(id("download")) class="btn sm" type="button" data-action="download"
                    data-kind=(k) hidden { "Download SVG" span class="vh" { " of your " (k) } }
                button id=(id("remove")) class="btn sm" type="button" data-action="remove"
                    data-kind=(k) hidden { "Remove" span class="vh" { " your " (k) } }
                button id=(id("relabel")) class="btn sm studio-relabel" type="button"
                    data-action="relabel" data-kind=(k) hidden { "This is actually a " (to) }
                // After Remove, until the slot holds a drawing again (in memory only).
                button id=(id("undo")) class="btn sm" type="button" data-action="undo"
                    data-kind=(k) hidden { "Undo" span class="vh" { " removing your " (k) } }
            }
            // Moving the upload over the other slot's own drawing needs a second tap here.
            div id=(id("confirm")) class="studio-confirm" role="group"
                aria-labelledby=(id("confirm-text")) hidden {
                p id=(id("confirm-text")) {
                    "Your " (to) " slot already has a drawing. Replace it with this one?"
                }
                div class="studio-confirm-buttons" {
                    button id=(id("confirm-yes")) class="btn sm solid" type="button"
                        data-action="relabel-confirm" data-kind=(k) { "Replace your " (to) }
                    button id=(id("confirm-no")) class="btn sm" type="button"
                        data-action="relabel-cancel" data-kind=(k) { "Cancel" }
                }
            }
        }
    }
}

/// The checks of one slot's upload, under its own heading, with its own pass state.
fn checks(kind: AssetKind) -> Markup {
    let k = kind_word(kind);
    let id = |part: &str| slot_id(part, kind);
    html! {
        div id=(id("checks")) class="studio-checks" data-kind=(k) data-testid=(id("checks"))
            role="group" aria-labelledby=(id("checks-title")) hidden {
            h3 id=(id("checks-title")) class="studio-checks-title" { (title_word(kind)) }
            p id=(id("pass")) class="studio-pass" data-testid=(id("pass")) hidden {
                span class="studio-lint-icon" aria-hidden="true" { "✓" }
                span { "Passes every check the official " (k) "s pass." }
            }
            ul id=(id("warnings")) class="studio-lint-list" data-testid=(id("warnings")) {}
            ul id=(id("tips")) class="studio-lint-list" data-testid=(id("tips")) {}
            details id=(id("details")) class="studio-details" hidden {
                summary id=(id("details-summary")) { "Details (0)" }
                ul id=(id("info")) class="studio-lint-list" data-testid=(id("info")) {}
            }
        }
    }
}

/// Before the first upload: where to get a template, how to draw, and a finished
/// example to run through the studio. Under "Your snake"; open until the first upload,
/// then studio.js closes it (it stays one tap away).
fn start_here() -> Markup {
    html! {
        details #studio-start class="studio-panel studio-start" data-testid="studio-start" open {
            summary class="studio-start-summary" {
                h2 { "Start here" }
                span class="studio-muted studio-start-hint" { "Templates, the guide and an example" }
            }
            ol class="studio-start-steps" {
                li {
                    h3 { "Get a template" }
                    p class="studio-muted" {
                        "A 1000 × 1000 px canvas with the guides on their own layer."
                    }
                    @for kind in [AssetKind::Head, AssetKind::Tail] {
                        div class="studio-start-kind" {
                            span class="studio-start-kind-name" { (title_word(kind)) }
                            div class="studio-start-buttons" {
                                @for d in templates(kind) {
                                    (download_button(d, kind, "btn"))
                                }
                            }
                        }
                    }
                }
                li {
                    h3 { "Draw" }
                    p {
                        "In black on the \u{201c}Draw here\u{201d} layer: one solid shape, edge "
                        "to edge, with holes for the eyes and mouth."
                    }
                    a class="btn" href=(GUIDE_PATH) { "Read the guide" }
                }
                li {
                    h3 { "Upload it" }
                    p {
                        "Hide the guides, export a PNG (or an SVG), and upload it with the "
                        "head card or the tail card above."
                    }
                }
            }
            div class="studio-start-example" {
                button #studio-example class="btn" type="button"
                    data-src=(asset_url(EXAMPLE_DRAWING)) { "Try an example" }
                span class="studio-muted" { "A finished head, run through the studio." }
            }
        }
    }
}

/// One slot's asset at large size over a checkerboard, with a translucent body stub
/// where the body joins (the left edge, for heads and tails alike) and room for red
/// brackets on gaps in it.
fn closeup(kind: AssetKind) -> Markup {
    let shape = default_ref(kind);
    let k = kind_word(kind);
    let id = |part: &str| slot_id(part, kind);
    let checker = id("checker");
    html! {
        figure class="studio-closeup-fig" {
            figcaption class="studio-closeup-title" { (title_word(kind)) }
            svg id=(id("closeup")) class="studio-closeup" xmlns="http://www.w3.org/2000/svg"
                viewBox="-24 -6 130 112" role="img" aria-label={ "Close-up of the default " (k) } {
                defs {
                    pattern id=(checker) width="10" height="10" patternUnits="userSpaceOnUse" {
                        rect width="10" height="10" fill="#ece9f1" {}
                        rect width="5" height="5" fill="#dcd7e4" {}
                        rect x="5" y="5" width="5" height="5" fill="#dcd7e4" {}
                    }
                }
                rect class="studio-closeup-bg" width="100" height="100"
                    fill={ "url(#" (checker) ")" } {}
                rect class="studio-closeup-body" x="-24" width="24" height="100" {}
                path id=(id("closeup-path")) class="studio-closeup-path" d=(shape.d)
                    fill-rule=(shape.fill_rule.as_svg()) {}
                g id=(id("gaps")) class="studio-gaps" {}
                rect class="studio-closeup-frame" width="100" height="100" fill="none" {}
            }
        }
    }
}
