use axum::{
    extract::RawQuery,
    response::{IntoResponse, Redirect},
    routing::get,
};

use crate::state::AppState;

/// Community short links carried over from play.battlesnake.com. These paths
/// are pinned all over Discord, READMEs, and docs, so they need to keep
/// working after cutover.
pub const REDIRECTS: &[(&str, &str)] = &[
    // Sponsors
    ("/sponsor", "https://github.com/sponsors/BattlesnakeOfficial"),
    ("/sponsors", "https://github.com/sponsors/BattlesnakeOfficial"),
    // Docs
    ("/docs", "https://docs.battlesnake.com"),
    ("/faq", "https://docs.battlesnake.com/faq"),
    (
        "/feedback",
        "https://github.com/BattlesnakeOfficial/feedback/discussions",
    ),
    // Blog
    ("/blog", "https://docs.battlesnake.com/blog"),
    // Socials
    ("/discord", "https://discord.gg/BYubeHQ"),
    ("/facebook", "https://www.facebook.com/playbattlesnake/"),
    ("/github", "https://github.com/battlesnakeofficial"),
    ("/instagram", "https://www.instagram.com/battlesnakeofficial/"),
    ("/reddit", "https://www.reddit.com/r/battlesnake"),
    ("/twitch", "https://twitch.tv/BattlesnakeOfficial"),
    ("/twitter", "https://twitter.com/playbattlesnake"),
    ("/youtube", "https://www.youtube.com/battlesnake"),
];

/// Register every short-link route on the given router.
pub fn register(mut router: axum::Router<AppState>) -> axum::Router<AppState> {
    for (path, target) in REDIRECTS {
        router = router.route(
            path,
            get(move |RawQuery(query): RawQuery| async move {
                redirect_to(target, query.as_deref())
            }),
        );
    }
    router
}

/// Temporary (not permanent) redirects: the targets are external and can
/// change (Discord invites, social handles) without us wanting browsers to
/// cache the old destination forever. Query strings pass through so tracking
/// params on shared links survive.
fn redirect_to(target: &str, query: Option<&str>) -> impl IntoResponse + use<> {
    match query {
        Some(q) if !q.is_empty() => Redirect::temporary(&format!("{target}?{q}")),
        _ => Redirect::temporary(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header::LOCATION};

    #[test]
    fn all_redirect_targets_are_absolute_urls() {
        for (path, target) in REDIRECTS {
            assert!(path.starts_with('/'), "{path} must start with /");
            assert!(
                target.starts_with("https://"),
                "{target} must be an absolute https URL"
            );
        }
    }

    #[test]
    fn redirect_preserves_query_string() {
        let response = redirect_to("https://example.com", Some("utm_source=discord"))
            .into_response();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers().get(LOCATION).unwrap(),
            "https://example.com?utm_source=discord"
        );
    }

    #[test]
    fn redirect_without_query_uses_bare_target() {
        let response = redirect_to("https://example.com", None).into_response();
        assert_eq!(
            response.headers().get(LOCATION).unwrap(),
            "https://example.com"
        );
    }
}
