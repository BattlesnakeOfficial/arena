//! A snake's head, body and tail as one small inline SVG, for list rows
//! (leaderboard ranked + placement tables, "your snakes" rail, home
//! ladder preview). Replaces the plain colour chip: art comes from the
//! design-kit catalog via `og::board::shape` (same source as the social
//! cards), colour flows through the `--snake` CSS variable like the studio
//! board, and near-black colours get a `dim` class whose CSS adds a faint
//! light outline so they stay visible on the dark theme. Multi-colour
//! special art (beach-puffin-special, pirate-special, trans-rights-scarf, …)
//! also draws its fixed-colour layers (`design_kit::catalog_layers`) over
//! the base path, so accents keep their true colours as on the real board.
//! Layers carry an inline `fill` style because the stylesheet's
//! `--snake` fill rule beats presentation attributes.

use arena::design_kit::AssetKind;
use maud::{Markup, html};

use crate::customizations::normalize_color;

/// `head`/`tail` are declared slugs — empty or unknown slugs draw the
/// default shape (`og::board::shape` folds that in). `color` is the stored
/// declared colour; anything not strict `#rrggbb` falls back to the CSS
/// default (#888888, the old chip fallback). The SVG is `aria-hidden`:
/// rows repeat the snake's name as link text right next to it.
pub fn snake_avatar(head: &str, tail: &str, color: &str) -> Markup {
    let head = crate::og::board::shape(AssetKind::Head, head);
    let tail = crate::og::board::shape(AssetKind::Tail, tail);
    let color = normalize_color(color);
    let dim = crate::og::board::is_dark_hex(&color);
    let class = if dim { "snake dim" } else { "snake" };
    let style = (!color.is_empty()).then(|| format!("--snake: {color}"));
    html! {
        svg class="snake-avatar" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 100"
            aria-hidden="true" focusable="false" {
            g class=(class) style=[style] {
                @if let Some(tail) = tail {
                    // Tail art natively has the body on its left edge with
                    // the tip pointing right; mirror it so the tip faces
                    // left, away from the body (same Direction::Left
                    // transform as components::snake_board).
                    svg class="tail" viewBox="0 0 100 100" x="0" y="0" width="100" height="100" {
                        g transform="scale(-1,1) translate(-100,0)" {
                            path d=(tail.d) fill-rule=(tail.fill_rule.as_svg()) {}
                            @for layer in crate::og::board::layers(tail) {
                                path d=(layer.d) fill-rule=(layer.fill_rule.as_svg())
                                    style=(format!("fill: {}", layer.fill)) {}
                            }
                        }
                    }
                }
                rect class="body" x="90" y="0" width="120" height="100" {}
                @if let Some(head) = head {
                    // Head art faces right and attaches on its left edge — identity.
                    svg class="head" viewBox="0 0 100 100" x="200" y="0" width="100" height="100" {
                        path d=(head.d) fill-rule=(head.fill_rule.as_svg()) {}
                        @for layer in crate::og::board::layers(head) {
                            path d=(layer.d) fill-rule=(layer.fill_rule.as_svg())
                                style=(format!("fill: {}", layer.fill)) {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(head: &str, tail: &str, color: &str) -> String {
        snake_avatar(head, tail, color).into_string()
    }

    fn head_d(slug: &str) -> &'static str {
        crate::og::board::shape(AssetKind::Head, slug)
            .unwrap_or_else(|| panic!("{slug:?} not in catalog"))
            .d
    }

    fn tail_d(slug: &str) -> &'static str {
        crate::og::board::shape(AssetKind::Tail, slug)
            .unwrap_or_else(|| panic!("{slug:?} not in catalog"))
            .d
    }

    #[test]
    fn custom_head_and_tail_render_their_catalog_paths() {
        let html = body("alligator", "bolt", "#FF4F86");
        assert!(html.contains(head_d("alligator")), "head path missing");
        assert!(html.contains(tail_d("bolt")), "tail path missing");
        assert!(
            html.contains("style=\"--snake: #ff4f86\""),
            "colour not lowered and set as --snake: {html}"
        );
        assert!(html.contains("class=\"snake\""));
        assert!(!html.contains("dim"));
    }

    #[test]
    fn empty_and_unknown_slugs_fall_back_to_default() {
        let default_head = head_d("default");
        let default_tail = tail_d("default");
        for (head, tail) in [("", ""), ("not-a-slug", "also-not-a-slug")] {
            let html = body(head, tail, "#888888");
            assert!(
                html.contains(default_head),
                "head default missing for {head:?}"
            );
            assert!(
                html.contains(default_tail),
                "tail default missing for {tail:?}"
            );
        }
    }

    #[test]
    fn invalid_color_omits_the_style() {
        let html = body("alligator", "bolt", "red; background:url(x)");
        assert!(
            html.contains("<g class=\"snake\">"),
            "plain snake group missing: {html}"
        );
        assert!(
            !html.contains("style="),
            "invalid colour must not reach CSS"
        );
        assert!(
            !html.contains("dim"),
            "empty normalized colour is never dark"
        );
    }

    #[test]
    fn dark_color_adds_dim_class() {
        assert!(body("alligator", "bolt", "#25272b").contains("class=\"snake dim\""));
        assert!(!body("alligator", "bolt", "#FF4F86").contains("class=\"snake dim\""));
    }

    #[test]
    fn avatar_is_hidden_from_assistive_tech() {
        assert!(body("alligator", "bolt", "#FF4F86").contains("aria-hidden=\"true\""));
    }

    #[test]
    fn svg_tags_are_balanced() {
        // maud's `;` leaf form emits an unclosed start tag; the HTML parser
        // then nests every following sibling inside it and each shape
        // collapses to 0×0 with no error anywhere else. This string count
        // is the only guard.
        for html in [
            body("alligator", "bolt", "#FF4F86"),
            body("", "", ""),
            body("beach-puffin-special", "beach-puffin-special", "#da8a1a"),
        ] {
            for tag in ["path", "rect"] {
                let open = html.matches(&format!("<{tag}")).count();
                let close = html.matches(&format!("</{tag}>")).count();
                assert!(open > 0, "no <{tag}> rendered");
                assert_eq!(open, close, "unbalanced <{tag}> tags: {html}");
            }
        }
    }

    #[test]
    fn multi_colour_catalog_art_keeps_its_fixed_colours() {
        // Acceptance pins, not observations: the puffin's orange beak and
        // white face, and the scarf's blue stripe. If the generated table
        // lacks one, fix the extractor — never relax these colours.
        let puffin = body("beach-puffin-special", "beach-puffin-special", "#da8a1a");
        assert!(
            puffin.contains(r#"style="--snake: #da8a1a""#),
            "base colour lost: {puffin}"
        );
        assert!(
            puffin.contains(r#"style="fill: #e84600""#),
            "puffin beak #e84600 missing: {puffin}"
        );
        assert!(
            puffin.contains(r#"style="fill: #ffffff""#),
            "puffin white face missing: {puffin}"
        );

        let scarf = body("trans-rights-scarf", "", "#25272b");
        assert!(
            scarf.contains(r#"style="fill: #75cdfd""#),
            "scarf stripe #75cdfd missing: {scarf}"
        );
        assert!(
            scarf.contains(r#"class="snake dim""#),
            "dim lost with layers: {scarf}"
        );
    }

    #[test]
    fn single_colour_catalog_art_gets_no_layers() {
        // cat is white-only (the white-composite gate); alligator/bolt are
        // plain single-colour art. Both render exactly as before.
        for html in [
            body("cat", "", "#da8a1a"),
            body("alligator", "bolt", "#ff4f86"),
        ] {
            assert!(
                !html.contains("style=\"fill: #"),
                "single-colour art grew a layer: {html}"
            );
        }
    }

    #[test]
    fn every_layer_path_is_emitted() {
        let html = body("beach-puffin-special", "beach-puffin-special", "#da8a1a");
        for kind in [AssetKind::Head, AssetKind::Tail] {
            let shape = crate::og::board::shape(kind, "beach-puffin-special")
                .expect("beach-puffin-special in catalog");
            let layers = crate::og::board::layers(shape);
            assert!(!layers.is_empty(), "{kind:?} has no layers");
            for layer in layers {
                assert!(
                    html.contains(layer.d),
                    "{kind:?} {} layer path missing",
                    layer.fill
                );
            }
        }
    }
}
