//! Country flag badge rendering for owner lines and profile meta.

use maud::{Markup, html};

/// Flag badge `<img>` for an ISO alpha-2 country code. Renders NOTHING
/// (empty Markup) when the code is empty or unknown: `name_for` doubles
/// as the membership guard because `asset_url` panics on unknown asset
/// names. Takes a CODE — it does not name-normalize.
pub fn country_flag(code: &str) -> Markup {
    let Some(name) = crate::countries::name_for(code) else {
        return html! {};
    };
    html! {
        img.flag alt={(name) " flag"} title={(name)} width="16" height="12"
            decoding="async"
            src=(crate::arena_assets::asset_url(&format!(
                "flags/{}.svg",
                code.trim().to_lowercase()
            ))) {}
    }
}

/// `country_flag(code) + " " + name` for profile meta lines; empty
/// Markup when unset/unknown.
pub fn country_meta(code: &str) -> Markup {
    let Some(name) = crate::countries::name_for(code) else {
        return html! {};
    };
    html! { (country_flag(code)) " " (name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_flag_renders_img_for_known_code() {
        let html = country_flag("CA").into_string();
        assert!(html.contains("<img class=\"flag\""), "{html}");
        assert!(html.contains("alt=\"Canada flag\""), "{html}");
        assert!(html.contains("title=\"Canada\""), "{html}");
        assert!(html.contains("/assets/"), "{html}");
        let src = html
            .split("src=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(src.ends_with(".svg"), "src: {src}");
    }

    #[test]
    fn country_flag_empty_for_unset_unknown_or_name() {
        assert_eq!(country_flag("").into_string(), "");
        assert_eq!(country_flag("XX").into_string(), "");
        assert_eq!(country_flag("Canada").into_string(), "");
    }

    #[test]
    fn country_meta_renders_flag_and_name() {
        let html = country_meta("CA").into_string();
        assert!(html.contains("img class=\"flag\""), "{html}");
        assert!(html.contains("Canada"), "{html}");
        assert_eq!(country_meta("").into_string(), "");
        assert_eq!(country_meta("XX").into_string(), "");
    }
}
