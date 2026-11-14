//! A snake's head, body and tail as one small inline SVG, for list rows
//! (leaderboard ranked + placement tables, "your snakes" rail, home
//! ladder preview). Replaces the plain colour chip: art comes from the
//! design-kit catalog via `og::board::shape` (same source as the social
//! cards), colour flows through the `--snake` CSS variable like the studio
//! board, and near-black colours get a `dim` class whose CSS adds a faint
//! light outline so they stay visible on the dark theme.

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
                        }
                    }
                }
                rect class="body" x="90" y="0" width="120" height="100" {}
                @if let Some(head) = head {
                    // Head art faces right and attaches on its left edge — identity.
                    svg class="head" viewBox="0 0 100 100" x="200" y="0" width="100" height="100" {
                        path d=(head.d) fill-rule=(head.fill_rule.as_svg()) {}
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
        for html in [body("alligator", "bolt", "#FF4F86"), body("", "", "")] {
            for tag in ["path", "rect"] {
                let open = html.matches(&format!("<{tag}")).count();
                let close = html.matches(&format!("</{tag}>")).count();
                assert!(open > 0, "no <{tag}> rendered");
                assert_eq!(open, close, "unbalanced <{tag}> tags: {html}");
            }
        }
    }
}
