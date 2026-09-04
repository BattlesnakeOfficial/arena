use maud::{DOCTYPE, Markup, PreEscaped, Render, html};

use crate::{config::LOCAL_BASE_URL, models::user::User, static_assets::asset_url};

/// Resolves the two theme axes before first paint so there is no flash of
/// the wrong theme. Mirrors the logic in /static/theme.js: html data
/// attributes (logged-in account setting) win over localStorage (anonymous).
const THEME_BOOTSTRAP_JS: &str = r#"(function(){var d=document.documentElement;function p(a,f){return d.getAttribute("data-bs-"+a)||localStorage.getItem("bs-"+a)||f}var site=p("site","system");var th=p("theater","dark");var sys=matchMedia("(prefers-color-scheme: dark)").matches?"dark":"light";var s=site==="system"?sys:site;d.setAttribute("data-app-theme",d.hasAttribute("data-theater-page")?(th==="match"?s:th):s);})();"#;

/// Site-wide fallback for social embeds (OpenGraph/Twitter) when a page
/// doesn't set its own description.
const DEFAULT_DESCRIPTION: &str = "A competitive arena where your code battles other Battlesnakes.";

const GOOGLE_FONTS_HREF: &str = "https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,300;12..96,500;12..96,600;12..96,700;12..96,800&family=Instrument+Sans:ital,wght@0,400;0,500;0,600;1,400&family=IBM+Plex+Mono:wght@400;500;600&display=swap";

/// Primary nav links: (label, href, authed_only). "Snakes" is the public
/// directory of every public snake and "Players" the directory of the people
/// behind them; "My Snakes" manages your own and is only shown logged in.
const NAV_LINKS: [(&str, &str, bool); 6] = [
    ("Leaderboards", "/leaderboards", false),
    ("Tournaments", "/tournaments", false),
    ("Snakes", "/snakes", false),
    ("Players", "/players", false),
    ("My Snakes", "/battlesnakes", true),
    ("Customizations", "/customizations", false),
];

pub struct Page {
    pub title: String,
    pub content: Box<dyn Render>,
    pub flash: Option<String>,
    pub flash_type: Option<String>,
    pub user: Option<User>,
    /// Request path, used to highlight the active nav link.
    pub current_path: String,
    pub base_url: String,
    /// Theater pages (game live/replay) resolve their theme from the
    /// theater axis instead of the site axis.
    pub theater: bool,
    /// Page-specific description for social embeds (OpenGraph/Twitter).
    /// Falls back to a site-wide default when unset.
    pub description: Option<String>,
}

impl Page {
    pub fn new(title: String, content: Box<dyn Render>, flash: Option<String>) -> Self {
        Self {
            title,
            content,
            flash,
            flash_type: None,
            user: None,
            current_path: "/".to_string(),
            base_url: LOCAL_BASE_URL.to_string(),
            theater: false,
            description: None,
        }
    }

    /// Set a page-specific social-embed description (builder style, so it
    /// chains off `PageFactory::create_page` / `create_theater_page`).
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    fn description(&self) -> &str {
        self.description.as_deref().unwrap_or(DEFAULT_DESCRIPTION)
    }

    fn canonical_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.current_path
        )
    }

    fn social_image_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            asset_url("og-card.png")
        )
    }

    /// Server-rendered initial theme, when it can be known without JS.
    /// `None` means "depends on prefers-color-scheme" — the bootstrap
    /// script resolves it before paint (no-JS visitors get light).
    fn initial_theme(&self) -> Option<&'static str> {
        let user = self.user.as_ref()?;
        let axis = if self.theater {
            match user.theater_theme.as_str() {
                "match" => user.site_theme.as_str(),
                explicit => explicit,
            }
        } else {
            user.site_theme.as_str()
        };
        match axis {
            "light" => Some("light"),
            "dark" => Some("dark"),
            _ => None,
        }
    }

    fn is_active(&self, href: &str) -> bool {
        self.current_path == href || self.current_path.starts_with(&format!("{href}/"))
    }

    fn wordmark(&self) -> Markup {
        html! {
            a class="wordmark" href="/" aria-label="Battlesnake Arena — home" {
                svg class="glyph" viewBox="0 0 24 24" fill="none" aria-hidden="true" {
                    rect x="2" y="2" width="9" height="9" rx="2.5" fill="var(--pink)" {}
                    rect x="13" y="2" width="9" height="9" rx="2.5" fill="currentColor" opacity=".2" {}
                    rect x="2" y="13" width="9" height="9" rx="2.5" fill="currentColor" opacity=".2" {}
                    rect x="13" y="13" width="9" height="9" rx="2.5" fill="var(--pink)" opacity=".45" {}
                }
                span class="wordmark-text" { "Battlesnake" }
            }
        }
    }

    fn nav_links(&self) -> Markup {
        html! {
            @for (label, href, authed_only) in NAV_LINKS {
                @if !authed_only || self.user.is_some() {
                    a class=[self.is_active(href).then_some("active")] href=(href) { (label) }
                }
            }
            a href="https://docs.battlesnake.com" { "Docs" }
        }
    }

    fn theme_controls(&self) -> Markup {
        html! {
            div class="theme-ctl" {
                button class="theme-btn" id="theme-toggle" type="button" title="Toggle theme" aria-label="Toggle theme" {
                    svg class="ic-sun" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true" {
                        circle cx="8" cy="8" r="3.2" {}
                        path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.4 1.4M11.6 11.6L13 13M13 3l-1.4 1.4M4.4 11.6L3 13" {}
                    }
                    svg class="ic-moon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true" {
                        path d="M13.5 9.5A6 6 0 1 1 6.5 2.5a5 5 0 0 0 7 7z" {}
                    }
                }
                button class="theme-btn" id="appearance-btn" type="button" title="Appearance settings" aria-label="Appearance settings" {
                    // sliders icon — must read distinct from the sun/moon toggle next to it
                    svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" aria-hidden="true" {
                        path d="M2 4.5h12M2 8h12M2 11.5h12" {}
                        circle cx="10.5" cy="4.5" r="1.7" fill="var(--card)" {}
                        circle cx="5.5" cy="8" r="1.7" fill="var(--card)" {}
                        circle cx="11" cy="11.5" r="1.7" fill="var(--card)" {}
                    }
                }
            }
        }
    }

    fn appearance_popover(&self) -> Markup {
        html! {
            div class="appearance" id="appearance" hidden {
                h4 { "Appearance" }
                fieldset {
                    legend { "Site theme" }
                    label { input type="radio" name="site" value="system"; "System" }
                    label { input type="radio" name="site" value="light"; "Light" }
                    label { input type="radio" name="site" value="dark"; "Dark" }
                }
                fieldset {
                    legend { "Game theater" }
                    label { input type="radio" name="theater" value="match"; "Match site theme" }
                    label { input type="radio" name="theater" value="dark"; "Always dark" }
                    label { input type="radio" name="theater" value="light"; "Always light" }
                }
                p class="hint" {
                    @if self.user.is_some() {
                        "Saved to your account."
                    } @else {
                        "Saved in this browser. Sign in to sync across devices."
                    }
                }
            }
        }
    }

    fn nav(&self) -> Markup {
        html! {
            nav class="site-nav" {
                (self.wordmark())
                div class="links" { (self.nav_links()) }
                div class="spacer" {}
                (self.theme_controls())
                details class="mobile-menu" {
                    summary aria-label="Menu" {
                        svg viewBox="0 0 18 18" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true" {
                            path d="M2 4.5h14M2 9h14M2 13.5h14" {}
                        }
                    }
                    div class="sheet" {
                        (self.nav_links())
                        @if self.user.is_none() {
                            a class="cta" href="/auth/github" { "Sign in with GitHub" }
                        }
                    }
                }
                @if let Some(user) = &self.user {
                    a class="nav-user" href="/me" {
                        @if let Some(avatar) = &user.github_avatar_url {
                            img src=(avatar) alt="" width="34" height="34";
                        }
                        span { (user.display_name.as_deref().unwrap_or(&user.github_login)) }
                    }
                } @else {
                    a class="btn solid" href="/auth/github" { "Sign in with GitHub" }
                }
            }
        }
    }

    fn footer(&self) -> Markup {
        html! {
            footer class="site-footer" {
                div class="inner" {
                    span { "Battlesnake Arena" }
                    div class="spacer" {}
                    a href="/conduct" { "Code of Conduct" }
                    a href="/privacy" { "Privacy Policy" }
                    a href="/terms" { "Terms of Service" }
                }
            }
        }
    }
}

impl Render for Page {
    fn render(&self) -> Markup {
        html! {
            (DOCTYPE)
            html lang="en"
                data-app-theme=[self.initial_theme()]
                data-theater-page[self.theater]
                data-authed[self.user.is_some()]
                data-bs-site=[self.user.as_ref().map(|u| u.site_theme.as_str())]
                data-bs-theater=[self.user.as_ref().map(|u| u.theater_theme.as_str())]
            {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1.0";
                    title { (self.title) " — Battlesnake" }
                    meta name="description" content=(self.description());
                    meta property="og:site_name" content="Battlesnake Arena";
                    meta property="og:title" content=(self.title);
                    meta property="og:type" content="website";
                    meta property="og:description" content=(self.description());
                    meta property="og:url" content=(self.canonical_url());
                    meta property="og:image" content=(self.social_image_url());
                    meta property="og:image:type" content="image/png";
                    meta property="og:image:width" content="1200";
                    meta property="og:image:height" content="630";
                    meta property="og:image:alt" content="Battlesnake Arena";
                    meta name="twitter:card" content="summary_large_image";
                    meta name="twitter:image" content=(self.social_image_url());
                    meta name="twitter:image:alt" content="Battlesnake Arena";
                    link rel="preconnect" href="https://fonts.googleapis.com";
                    link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                    link href=(GOOGLE_FONTS_HREF) rel="stylesheet";
                    link rel="stylesheet" href=(asset_url("arena.css"));
                    link rel="icon" href=(asset_url("favicon.svg")) type="image/svg+xml";
                    script { (PreEscaped(THEME_BOOTSTRAP_JS)) }
                    script src=(asset_url("viewTransition.js")) {}
                    script src=(asset_url("theme.js")) defer {}
                }

                body {
                    (self.nav())

                    @if let Some(flash_message) = &self.flash {
                        div class="flash-message" data-flash-type=[self.flash_type.as_deref()] {
                            (flash_message)
                        }
                    }

                    main {
                        div class="page" {
                            (self.content.render())
                        }
                    }

                    (self.footer())
                    (self.appearance_popover())
                }
            }
        }
    }
}

impl axum::response::IntoResponse for Page {
    fn into_response(self) -> axum::response::Response {
        self.render().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_page() -> Page {
        Page::new(
            "Test Title".to_string(),
            Box::new(html! { p { "content" } }),
            None,
        )
    }

    #[test]
    fn head_contains_baseline_og_tags() {
        let mut page = test_page();
        page.base_url = "https://arena.battlesnake.com/".to_string();
        page.current_path = "/games/example".to_string();
        let html = page.render().into_string();
        assert!(html.contains(r#"<meta property="og:site_name" content="Battlesnake Arena">"#));
        assert!(html.contains(r#"<meta property="og:title" content="Test Title">"#));
        assert!(html.contains(r#"<meta property="og:type" content="website">"#));
        assert!(html.contains(&format!(
            r#"<meta property="og:description" content="{DEFAULT_DESCRIPTION}">"#
        )));
        let image_url = format!("https://arena.battlesnake.com{}", asset_url("og-card.png"));
        let expected = [
            (
                r#"property="og:url""#,
                "https://arena.battlesnake.com/games/example",
            ),
            (r#"property="og:image""#, image_url.as_str()),
            (r#"property="og:image:type""#, "image/png"),
            (r#"property="og:image:width""#, "1200"),
            (r#"property="og:image:height""#, "630"),
            (r#"property="og:image:alt""#, "Battlesnake Arena"),
            (r#"name="twitter:card""#, "summary_large_image"),
            (r#"name="twitter:image""#, image_url.as_str()),
            (r#"name="twitter:image:alt""#, "Battlesnake Arena"),
        ];
        for (selector, value) in expected {
            assert_eq!(html.matches(selector).count(), 1, "{selector}");
            assert!(html.contains(&format!(r#"{selector} content="{value}""#)));
        }
        assert!(!html.contains("arena.battlesnake.com//"));
        assert!(html.contains(&format!(
            r#"<meta name="description" content="{DEFAULT_DESCRIPTION}">"#
        )));
    }

    #[test]
    fn custom_description_overrides_default() {
        let html = test_page()
            .with_description("Standard game on an 11x11 board — watch the replay")
            .render()
            .into_string();
        assert!(html.contains(
            r#"<meta property="og:description" content="Standard game on an 11x11 board — watch the replay">"#
        ));
        assert!(!html.contains(DEFAULT_DESCRIPTION));
    }

    #[test]
    fn players_directory_is_in_the_anonymous_nav() {
        let html = test_page().render().into_string();
        // Rendered twice: the desktop link row and the mobile menu sheet.
        assert_eq!(html.matches(r#"href="/players">Players</a>"#).count(), 2);
    }

    #[test]
    fn players_nav_link_is_active_on_the_directory() {
        let mut page = test_page();
        page.current_path = "/players".to_string();
        let html = page.render().into_string();
        assert_eq!(
            html.matches(r#"<a class="active" href="/players">Players</a>"#)
                .count(),
            2
        );
    }

    #[test]
    fn description_is_html_escaped() {
        let html = test_page()
            .with_description(r#"<script>"quotes" & snakes</script>"#)
            .render()
            .into_string();
        assert!(html.contains("&lt;script&gt;&quot;quotes&quot; &amp; snakes&lt;/script&gt;"));
        assert!(!html.contains(r#"<script>"quotes""#));
    }

    #[test]
    fn og_card_generator_literals_match_page_constants() {
        let generator = include_str!("../../../e2e/scripts/generate-og-card.mjs");
        for (name, value) in [
            ("GOOGLE_FONTS_HREF", GOOGLE_FONTS_HREF),
            ("DEFAULT_DESCRIPTION", DEFAULT_DESCRIPTION),
        ] {
            assert!(
                generator.contains(value),
                "{name} changed; update the card generator when the Rust constants change"
            );
        }
    }
}
