use maud::{Markup, html};

use crate::models::tag::Tag;

/// Render a battlesnake's tags as flat chips, shared by the snake profile,
/// user profile, and leaderboard pages.
///
/// Category-agnostic by design: language and platform tags render with the
/// same class and styling. Tags render in the order supplied (the model
/// queries already order by category, then name) — never re-sort here.
///
/// An empty slice renders nothing at all: no wrapper, label, or placeholder.
pub fn snake_tag_chips(tags: &[Tag]) -> Markup {
    if tags.is_empty() {
        return html! {};
    }

    html! {
        div class="snake-tags" {
            @for t in tags {
                span class="snake-tag" { (t.name) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::tag::TagCategory;
    use chrono::Utc;
    use uuid::Uuid;

    fn tag(name: &str, category: TagCategory) -> Tag {
        Tag {
            tag_id: Uuid::new_v4(),
            name: name.into(),
            category,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn empty_tags_render_nothing() {
        let out = snake_tag_chips(&[]).into_string();
        assert!(!out.contains("snake-tags"));
        assert!(!out.contains("snake-tag"));
        assert!(out.is_empty());
    }

    #[test]
    fn tags_render_in_supplied_order() {
        let tags = vec![
            tag("Rust", TagCategory::Language),
            tag("Fly.io", TagCategory::Platform),
        ];
        let out = snake_tag_chips(&tags).into_string();

        let rust_at = out.find("Rust").expect("Rust chip missing");
        let fly_at = out.find("Fly.io").expect("Fly.io chip missing");
        assert!(rust_at < fly_at, "tags must render in slice order: {out}");
    }

    #[test]
    fn categories_render_identically() {
        let tags = vec![
            tag("Rust", TagCategory::Language),
            tag("AWS", TagCategory::Platform),
        ];
        let out = snake_tag_chips(&tags).into_string();

        assert_eq!(out.matches("class=\"snake-tag\"").count(), 2);
        assert!(!out.contains("bg-info"));
        assert!(!out.contains("bg-secondary"));
    }
}
