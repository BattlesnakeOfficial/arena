use maud::{Markup, html};

pub fn user_avatar(avatar_url: Option<&str>, login: &str, context_class: &str) -> Markup {
    let fallback = login
        .chars()
        .next()
        .and_then(|initial| initial.to_uppercase().next())
        .unwrap_or('?');

    html! {
        span class={ "user-avatar " (context_class) } aria-hidden="true" {
            span class="avatar-fallback" { (fallback) }
            @if let Some(url) = avatar_url.filter(|url| !url.trim().is_empty()) {
                // This inline fallback depends on CSP allowing inline handlers. A future CSP
                // must use delegated script behavior or explicitly permit this handler.
                img src=(url) alt="" onerror="this.remove()";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::user_avatar;

    #[test]
    fn renders_decorative_escaped_image_and_fallback() {
        let rendered =
            user_avatar(Some("https://example.com/a?x=\"<&"), "alice", "nav-avatar").into_string();

        assert_eq!(rendered.matches("<img ").count(), 1);
        assert!(rendered.contains("<span class=\"avatar-fallback\">A</span>"));
        assert!(rendered.contains("src=\"https://example.com/a?x=&quot;&lt;&amp;\""));
        assert!(rendered.contains("alt=\"\""));
        assert!(rendered.contains("onerror=\"this.remove()\""));
    }

    #[test]
    fn omits_missing_and_blank_urls() {
        for url in [None, Some(""), Some("   ")] {
            let rendered = user_avatar(url, "alice", "avatar").into_string();
            assert!(!rendered.contains("<img "));
            assert!(!rendered.contains("src=\"\""));
        }
    }

    #[test]
    fn derives_one_fallback_glyph() {
        for (login, expected) in [("", "?"), ("alice", "A"), ("ßeta", "S"), ("éclair", "É")] {
            let rendered = user_avatar(None, login, "avatar").into_string();
            assert!(rendered.contains(&format!(
                "<span class=\"avatar-fallback\">{expected}</span>"
            )));
        }
    }

    #[test]
    fn escapes_login_url_and_context_class() {
        let rendered =
            user_avatar(Some("https://example.com/<&\""), "<&", "avatar \"<&").into_string();

        assert!(rendered.contains("class=\"user-avatar avatar &quot;&lt;&amp;\""));
        assert!(rendered.contains("src=\"https://example.com/&lt;&amp;&quot;\""));
        assert!(rendered.contains("<span class=\"avatar-fallback\">&lt;</span>"));
    }
}
