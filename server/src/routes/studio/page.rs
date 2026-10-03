//! `GET /customizations/studio`: the Head & Tail Studio page.
//!
//! Everything is server-rendered: every preview board (with the default head and tail
//! in place), the reference shapes for "Pair with", and empty containers for the
//! checks. `static/studio.js` posts uploads to the processing endpoint and then only
//! sets attributes: `d`/`fill-rule` on the placeholder paths, the `--studio-snake`
//! colour variable, the board theme class, and the text of the checks.
//!
//! Placeholder paths: every board's head is `path.studio-head` and every tail
//! `path.studio-tail`, whichever of them is the artist's own; the close-up is
//! `path#studio-closeup-path`.

use arena::design_kit::{
    AssetKind, FillRule, ProcessError, SIGNATURES, Sniffed,
    refs::{REFS, RefShape},
};
use axum::response::IntoResponse;
use maud::{Markup, html};
use serde::Serialize;

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
fn default_ref(kind: AssetKind) -> &'static RefShape {
    refs(kind)
        .find(|r| r.slug == "default")
        .or_else(|| refs(kind).next())
        .unwrap_or(&FALLBACK_REF)
}

/// The browser's instant check before an upload, rendered from the server's own rules
/// (`data-sniff` on `#studio-drop`): the formats [`design_kit::sniff`] rejects, with the
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
    let default_head = default_ref(AssetKind::Head);
    let label_all = "Four pink snakes wearing the default head and the default tail, facing \
                     right, left, up and down";
    // Without the check the browser just posts every file; the server answers anyway.
    let sniff = serde_json::to_string(&client_sniff()).unwrap_or_default();
    html! {
        div #studio .studio data-testid="studio" {
            div class="page-head" {
                h1 { "Head & Tail Studio" }
                div class="sub" {
                    "Upload a head or tail you drew and see it on a real Battlesnake board: "
                    "any colour, every direction, at game size and close up."
                }
            }
            noscript {
                p class="studio-noscript" { "The studio needs JavaScript to process your drawing." }
            }

            div class="studio-layout" {
                section class="studio-panel studio-upload" aria-labelledby="studio-upload-heading" {
                    h2 #studio-upload-heading class="vh" { "Upload" }
                    // Picks the slot the next upload fills, and which slot the close-up
                    // and the checks show.
                    fieldset #studio-kind class="studio-seg studio-kind" {
                        legend { "Upload as" }
                        div class="studio-seg-opts" {
                            label class="studio-seg-opt" {
                                input type="radio" name="studio-kind" value="head" checked;
                                span { "Head" }
                            }
                            label class="studio-seg-opt" {
                                input type="radio" name="studio-kind" value="tail";
                                span { "Tail" }
                            }
                        }
                    }
                    label #studio-drop class="studio-drop" data-testid="studio-drop" data-sniff=(sniff) {
                        input #studio-file class="studio-file" type="file" name="file"
                            accept=".png,.jpg,.jpeg,.svg,image/png,image/jpeg,image/svg+xml";
                        span #studio-drop-title class="studio-drop-title" { "Choose a drawing" }
                        span class="studio-drop-hint" { "PNG, JPEG or SVG, or drop it here" }
                    }
                    p #studio-status class="studio-status" role="status" aria-live="polite"
                        data-testid="studio-status" { "Try it with your own drawing." }
                    button #studio-relabel class="btn sm studio-relabel" type="button" hidden {
                        "Use it as a tail instead"
                    }
                    div #studio-top-warning class="studio-top-warning" data-testid="studio-top-warning" hidden {
                        span class="studio-lint-icon" aria-hidden="true" { "!" }
                        p #studio-top-warning-text {}
                        button #studio-top-fix class="btn sm studio-fix" type="button" hidden {}
                    }
                }

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
                            (closeup(default_head))
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
                        "Upload a drawing to check it against the rules every official head and tail follows."
                    }
                    p #studio-pass class="studio-pass" data-testid="studio-pass" hidden {
                        span class="studio-lint-icon" aria-hidden="true" { "✓" }
                        span #studio-pass-text { "Passes every check the official heads pass." }
                    }
                    ul #studio-warnings class="studio-lint-list" data-testid="studio-warnings" {}
                    ul #studio-tips class="studio-lint-list" data-testid="studio-tips" {}
                    details #studio-details class="studio-details" hidden {
                        summary #studio-details-summary { "Details (0)" }
                        ul #studio-info class="studio-lint-list" data-testid="studio-info" {}
                    }
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
                    (pair_select(AssetKind::Tail, "studio-pair-tail", "Pair your head with", false))
                    (pair_select(AssetKind::Head, "studio-pair-head", "Pair your tail with", true))
                }

                section class="studio-panel studio-actions" aria-labelledby="studio-actions-heading" {
                    h2 #studio-actions-heading class="vh" { "Save" }
                    div class="studio-action-row" {
                        button #studio-new-version class="btn solid" type="button" hidden { "Upload a new version" }
                        button #studio-download-head class="btn" type="button" hidden { "Download head SVG" }
                        button #studio-download-tail class="btn" type="button" hidden { "Download tail SVG" }
                        button #studio-save-image class="btn" type="button" { "Save preview image" }
                        button #studio-clear class="btn" type="button" hidden { "Clear" }
                    }
                    p class="studio-privacy studio-muted" {
                        "Your file is processed and immediately discarded on our servers. "
                        "Your latest preview is kept only in this browser."
                    }
                }
            }
        }
        script src=(asset_url("studio.js")) defer {}
    }
}

/// "Pair with" for the slot of `kind`: every reference of that kind, carrying its path
/// for the page to swap in. studio.js adds "Your head"/"Your tail" once that slot holds
/// an upload.
fn pair_select(kind: AssetKind, id: &str, label: &str, hidden: bool) -> Markup {
    let kind_name = match kind {
        AssetKind::Head => "head",
        AssetKind::Tail => "tail",
    };
    html! {
        div class="field studio-pair" id={ (id) "-field" } hidden[hidden] {
            label for=(id) { (label) }
            select id=(id) class="studio-pair-select" data-kind=(kind_name) {
                @for r in refs(kind) {
                    option value=(r.slug) data-d=(r.d) data-fill-rule=(r.fill_rule.as_svg())
                        selected[r.slug == "default"] { (r.display_name) }
                }
            }
        }
    }
}

/// The asset at large size over a checkerboard, with a translucent body stub where the
/// body joins (the left edge) and room for red brackets on gaps in it.
fn closeup(shape: &RefShape) -> Markup {
    html! {
        svg #studio-closeup class="studio-closeup" xmlns="http://www.w3.org/2000/svg"
            viewBox="-24 -6 130 112" role="img" aria-label="Close-up of the default head" {
            defs {
                pattern #studio-checker width="10" height="10" patternUnits="userSpaceOnUse" {
                    rect width="10" height="10" fill="#ece9f1" {}
                    rect width="5" height="5" fill="#dcd7e4" {}
                    rect x="5" y="5" width="5" height="5" fill="#dcd7e4" {}
                }
            }
            rect class="studio-closeup-bg" width="100" height="100" fill="url(#studio-checker)" {}
            rect class="studio-closeup-body" x="-24" width="24" height="100" {}
            path #studio-closeup-path class="studio-closeup-path" d=(shape.d)
                fill-rule=(shape.fill_rule.as_svg()) {}
            g #studio-gaps class="studio-gaps" {}
            rect class="studio-closeup-frame" width="100" height="100" fill="none" {}
        }
    }
}
