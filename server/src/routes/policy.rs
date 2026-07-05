use axum::http::StatusCode;
use axum::response::IntoResponse;
use maud::{Markup, html};

use crate::{components::page_factory::PageFactory, errors::ServerResult};

const EFFECTIVE_DATE: &str = "July 2026";

pub async fn conduct_page(
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Code of Conduct".to_string(), Box::new(conduct_content())))
}

pub async fn privacy_page(
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Privacy Policy".to_string(), Box::new(privacy_content())))
}

pub async fn terms_page(page_factory: PageFactory) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Terms of Service".to_string(), Box::new(terms_content())))
}

fn conduct_content() -> Markup {
    html! {
        div style="max-width: 800px; margin: 0 auto;" {
            h1 { "Code of Conduct" }
            p style="color: #666;" { "Effective: " (EFFECTIVE_DATE) }

            h2 { "Our Standards" }
            p {
                "Battlesnake Arena is a community where developers build and "
                "compete with programmable snakes. We expect everyone to be "
                "welcoming, respectful, and collaborative. Whether you're a "
                "beginner or a veteran, treat fellow competitors with kindness."
            }
            p {
                "Good participation includes:"
            }
            ul {
                li { "Being welcoming and inclusive of all skill levels and backgrounds." }
                li { "Showing respect for other players and their work." }
                li { "Engaging in the spirit of fair competition." }
                li { "Helping others on the Battlesnake Discord." }
            }

            h2 { "Unacceptable Behavior" }
            p {
                "The following behaviors are unacceptable in the Arena community:"
            }
            ul {
                li { "Harassment, discrimination, or intimidation of any kind." }
                li { "Cheating or abusing the platform — including flooding games, attacking other players' snake servers, or manipulating leaderboards." }
                li { "Posting abusive or offensive content in profiles or game names." }
                li { "Attempting to access or disrupt other users' accounts or data." }
            }

            h2 { "Enforcement" }
            p {
                "Arena moderators may take action when this Code of Conduct "
                "is violated. Actions may include removing content, revoking "
                "customization unlocks, or suspending accounts. Tournament "
                "results may be invalidated for cheating."
            }

            h2 { "Reporting" }
            p {
                "If you witness or experience a violation, report it on the "
                a href="https://discord.gg/BYubeHQ" { "Battlesnake Discord" }
                ". Include screenshots, game IDs, or other relevant context "
                "where possible. Reports are reviewed by the moderation team."
            }
        }
    }
}

fn privacy_content() -> Markup {
    html! {
        div style="max-width: 800px; margin: 0 auto;" {
            h1 { "Privacy Policy" }
            p style="color: #666;" { "Effective: " (EFFECTIVE_DATE) }

            h2 { "Information We Collect" }
            p {
                "When you sign in with GitHub, we receive and store your GitHub "
                "user ID, username (login), and avatar URL. If GitHub provides "
                "them, we also store your name and email, along with the OAuth "
                "tokens received during sign-in."
            }
            p {
                "If you create snakes, we store the name and URL you provide. "
                "Game history — including moves and outcomes — is stored for "
                "each game played."
            }

            h3 { "Play.battlesnake.com Migration Data" }
            p {
                "If you had an account on the old play.battlesnake.com, we "
                "imported your email, username, display name, profile fields "
                "(pronouns, country, backstory), and points balances to allow "
                "you to claim your account. Your play password was imported "
                "in hashed form only — it is never stored in plaintext, and "
                "it is checked only to verify a claim. When you claim your "
                "account, your play snakes and customization unlocks are "
                "merged into your Arena account."
            }

            h2 { "How We Use Your Information" }
            p {
                "We use your information to:"
            }
            ul {
                li { "Authenticate you and manage your session." }
                li { "Display your profile and snakes." }
                li { "Run games and store game results." }
                li { "Maintain leaderboards and tournament results." }
                li { "Send transactional email (via Mailgun) when your play account is claimed, as a security notice." }
                li { "Notify you by email if your snake is removed from leaderboard matchmaking because its server keeps failing." }
            }
            p {
                "We do not send marketing emails. We do not show "
                "advertisements. We do not sell your data. We share it only "
                "with the service providers that run Arena on our behalf — "
                "Mailgun for transactional email and Google Cloud for hosting "
                "and storage."
            }

            h2 { "Data Storage" }
            p {
                "Arena is hosted on Google Cloud. Game data from the legacy "
                "Battlesnake Engine may be archived to Google Cloud Storage."
            }

            h2 { "Cookies" }
            p {
                "Arena uses a single functional session cookie (HttpOnly, "
                "Secure, SameSite=Lax) to keep you logged in. We do not use "
                "tracking cookies, analytics cookies, or advertising cookies."
            }

            h2 { "Contact" }
            p {
                "Questions about privacy? Reach us on the "
                a href="https://discord.gg/BYubeHQ" { "Battlesnake Discord" }
                "."
            }
        }
    }
}

fn terms_content() -> Markup {
    html! {
        div style="max-width: 800px; margin: 0 auto;" {
            h1 { "Terms of Service" }
            p style="color: #666;" { "Effective: " (EFFECTIVE_DATE) }

            h2 { "Acceptance" }
            p {
                "By using Battlesnake Arena, you agree to these terms. If you "
                "do not agree, do not use the service."
            }

            h2 { "Your Account" }
            p {
                "Accounts are created and accessed through GitHub "
                "authentication. You are responsible for keeping your GitHub "
                "account secure."
            }

            h2 { "Your Content" }
            p {
                "You own the code and servers for your snakes. Arena stores "
                "the URL you provide and calls it during games. You are "
                "responsible for ensuring your snake server is available and "
                "does not violate these terms."
            }

            h2 { "Acceptable Use" }
            p {
                "Do not abuse the platform. This includes flooding games with "
                "requests, attacking other players' snake servers, or "
                "manipulating leaderboards. Arena may throttle or remove "
                "content, or suspend accounts that violate these terms."
            }
            p {
                "Rate limits apply to account claiming to prevent abuse. "
                "Infrastructure-level limits may also apply."
            }

            h2 { "Disclaimers" }
            p {
                "Arena is provided \u{201C}as is\u{201D} without warranties of "
                "any kind. Tournament and leaderboard results are public. "
                "Arena may modify or discontinue features at any time."
            }

            h2 { "Contact" }
            p {
                "Questions? Reach us on the "
                a href="https://discord.gg/BYubeHQ" { "Battlesnake Discord" }
                "."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conduct_content_renders_with_key_sections() {
        let html = conduct_content().into_string();
        assert!(html.contains("Code of Conduct"));
        assert!(html.contains("Our Standards"));
        assert!(html.contains("Unacceptable Behavior"));
        assert!(html.contains("Enforcement"));
        assert!(html.contains("Reporting"));
        assert!(html.contains(EFFECTIVE_DATE));
    }

    #[test]
    fn privacy_content_renders_with_key_sections() {
        let html = privacy_content().into_string();
        assert!(html.contains("Privacy Policy"));
        assert!(html.contains("Information We Collect"));
        assert!(html.contains("How We Use Your Information"));
        assert!(html.contains("Data Storage"));
        assert!(html.contains("Cookies"));
        assert!(html.contains("Contact"));
        assert!(html.contains(EFFECTIVE_DATE));
    }

    #[test]
    fn terms_content_renders_with_key_sections() {
        let html = terms_content().into_string();
        assert!(html.contains("Terms of Service"));
        assert!(html.contains("Acceptance"));
        assert!(html.contains("Your Account"));
        assert!(html.contains("Your Content"));
        assert!(html.contains("Acceptable Use"));
        assert!(html.contains("Disclaimers"));
        assert!(html.contains(EFFECTIVE_DATE));
    }

    #[test]
    fn footer_links_present_in_page_render() {
        use crate::components::page::Page;
        use maud::Render;
        let page = Page::new(
            "Test".to_string(),
            Box::new(html! { p { "content" } }),
            None,
        );
        let html = page.render().into_string();
        assert!(html.contains(r#"href="/conduct""#));
        assert!(html.contains(r#"href="/privacy""#));
        assert!(html.contains(r#"href="/terms""#));
    }
}
