//! Event copy and the shared HTML/plain-text email layout.

use color_eyre::eyre::eyre;
use maud::{Markup, PreEscaped, html};

use crate::notifications::NotificationKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailContent {
    pub subject: String,
    pub text: String,
    pub html_body: String,
}

impl EmailContent {
    pub fn from_text(subject: String, text: String) -> Self {
        let html_body = escaped_text(&text).into_string();
        Self {
            subject,
            text,
            html_body,
        }
    }
}

/// Preserve legacy line breaks, escaping all event data. Standalone HTTPS
/// action URLs become links; their exact href remains in the text alternative.
fn escaped_text(text: &str) -> Markup {
    html! {
        @for line in text.lines() {
            @if line.starts_with("https://") || line.starts_with("http://") {
                div style="margin:0 0 8px;overflow-wrap:anywhere" { a class="email-link" href=(line) style="color:#C11655;text-decoration:underline" { (line) } }
            } @else {
                div style="min-height:8px;margin:0 0 4px" { (line) }
            }
        }
    }
}

fn action_button(label: &str, url: &str) -> Markup {
    html! {
        table role="presentation" cellpadding="0" cellspacing="0" border="0" style="margin:24px 0 10px" {
            tr { td bgcolor="#C11655" style="background:#C11655;border-radius:6px" {
                a class="email-cta" href=(url) style="display:inline-block;background:#C11655;color:#FFFFFF;font-size:16px;font-weight:700;line-height:24px;padding:12px 20px;min-height:44px;box-sizing:border-box;text-decoration:none;border-radius:6px" { (label) }
            } }
        }
        p style="font-size:13px;line-height:1.5;overflow-wrap:anywhere;margin:0 0 24px" {
            "Or open this link: " a class="email-link" href=(url) style="color:#C11655;text-decoration:underline" { (url) }
        }
    }
}

/// A rendered, ready-to-send multipart message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub list_unsubscribe: Option<String>,
}

impl EmailMessage {
    pub fn form_fields<'a>(&'a self, from: &'a str) -> Vec<(&'static str, &'a str)> {
        let mut fields = vec![
            ("from", from),
            ("to", self.to.as_str()),
            ("subject", self.subject.as_str()),
            ("text", self.text.as_str()),
            ("html", self.html.as_str()),
        ];
        if let Some(url) = &self.list_unsubscribe {
            fields.push(("h:List-Unsubscribe", url));
            fields.push(("h:List-Unsubscribe-Post", "List-Unsubscribe=One-Click"));
        }
        fields
    }
}

pub fn notification_email_confirmation(url: &str) -> EmailContent {
    EmailContent {
        subject: "Confirm your Battlesnake notification email".to_string(),
        text: format!(
            "Confirm this address for optional Battlesnake Arena emails within 24 hours:\n\n{url}\n\nSign in to the same Arena account that requested this change. If your email app opens a signed-out browser, sign in and reopen this link. If you did not request this, you can ignore this email.\n"
        ),
        html_body: html! {
            p { "Confirm this address for optional Battlesnake Arena emails within 24 hours:" }
            (action_button("Confirm this address", url))
            p { "Sign in to the same Arena account that requested this change. If your email app opens a signed-out browser, sign in and reopen this link. If you did not request this, you can ignore this email." }
        }.into_string(),
    }
}

/// Sent to a play account's email when that account is claimed on arena
/// (via GitHub auto-link or password claim). Doubles as a security notice:
/// the real owner hears about a claim even if someone else initiated it.
pub fn account_claimed(
    play_username: &str,
    snakes_migrated: usize,
    grants_migrated: u64,
) -> EmailContent {
    let text = format!(
        "Hi {play_username},\n\
         \n\
         Your play.battlesnake.com account was just claimed on the new \
         Battlesnake Arena. We moved {snakes_migrated} snake(s) and \
         {grants_migrated} customization unlock(s) over to your account.\n\
         \n\
         If this was you, there's nothing to do — welcome back!\n\
         \n\
         If this WASN'T you, please reach out to us on Discord right away so \
         we can secure your account:\n\
         \n\
         https://arena.battlesnake.com/discord\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailContent {
        subject: "Your Battlesnake account was claimed".to_string(),
        text,
        html_body: html! {
            p { "Hi " (play_username) "," }
            p { "Your play.battlesnake.com account was just claimed on the new Battlesnake Arena. We moved " (snakes_migrated) " snake(s) and " (grants_migrated) " customization unlock(s) over to your account." }
            p { "If this was you, there's nothing to do — welcome back!" }
            p { "If this WASN'T you, please reach out to us on Discord right away so we can secure your account:" }
            p { a class="email-link" href="https://arena.battlesnake.com/discord" style="color:#C11655;text-decoration:underline" { "https://arena.battlesnake.com/discord" } }
            p { "— Battlesnake Arena" }
        }.into_string(),
    }
}

/// Sent to a snake's owner when the health sweeper pulls the snake from
/// matchmaking on one or more leaderboards after repeated failed probes.
/// Mirrors play's arena_matchmaking_deactivated notice: says what happened,
/// where, why, and how to get back in. `pulled` is `(leaderboard, most recent
/// problem)` per leaderboard, in the order to list them.
pub fn matchmaking_deactivated_content(
    snake_name: &str,
    pulled: &[(String, String)],
    profile_url: &str,
) -> EmailContent {
    let where_ = if pulled.len() == 1 {
        "this Arena leaderboard"
    } else {
        "these Arena leaderboards"
    };
    let problems: String = pulled
        .iter()
        .map(|(leaderboard, problem)| format!("- {leaderboard}: {problem}\n"))
        .collect();
    let text = format!(
        "Hi,\n\
         \n\
         Your Battlesnake \"{snake_name}\" has been temporarily removed from \
         matchmaking on {where_} because its server kept timing out or \
         erroring in our health checks there:\n\
         \n\
         {problems}\
         \n\
         We test each leaderboard with a game shaped like its matches (same \
         mode, board size and number of snakes), so a snake can be healthy on \
         one leaderboard and not another. Its ratings are safe — we stop \
         matching it so a broken server doesn't hurt its standing.\n\
         \n\
         We'll keep checking and put it back automatically once it passes \
         again. Or, once your snake is fixed, resume matchmaking right away \
         from its profile page:\n\
         \n\
         {profile_url}\n\
         \n\
         (The \"Test Snake\" button there plays the same games we do, one per \
         leaderboard.)\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailContent {
        subject: format!("{snake_name} was paused from Arena matchmaking"),
        text,
        html_body: html! {
            p { "Hi," }
            p { "Your Battlesnake \"" (snake_name) "\" has been temporarily removed from matchmaking on " (where_) " because its server kept timing out or erroring in our health checks there:" }
            ul style="padding-left:22px;margin:12px 0 24px" {
                @for (leaderboard, problem) in pulled {
                    li style="margin:0 0 8px" { (leaderboard) ": " (problem) }
                }
            }
            p { "We test each leaderboard with a game shaped like its matches (same mode, board size and number of snakes), so a snake can be healthy on one leaderboard and not another. Its ratings are safe — we stop matching it so a broken server doesn't hurt its standing." }
            p { "We'll keep checking and put it back automatically once it passes again. Or, once your snake is fixed, resume matchmaking right away from its profile page:" }
            (action_button("Open snake profile", profile_url))
            p { "(The \"Test Snake\" button there plays the same games we do, one per leaderboard.)" }
            p { "— Battlesnake Arena" }
        }.into_string(),
    }
}

/// Sent when the health sweeper puts a snake's previously paused entries
/// back into matchmaking on its own, after enough consecutive healthy
/// probes. Closes the loop on [`matchmaking_deactivated_content`] so the owner isn't
/// left thinking they still need to press Resume.
pub fn matchmaking_reactivated_content(
    snake_name: &str,
    leaderboards: &[String],
    profile_url: &str,
) -> EmailContent {
    let leaderboards_text = leaderboards.join(", ");
    let text = format!(
        "Hi,\n\
         \n\
         Good news — your Battlesnake \"{snake_name}\" is passing our health \
         checks again and has been put back into Arena matchmaking \
         automatically on: {leaderboards_text}. No action needed.\n\
         \n\
         You can see its status any time on its profile page:\n\
         \n\
         {profile_url}\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailContent {
        subject: format!("{snake_name} is back in Arena matchmaking"),
        text,
        html_body: html! {
            p { "Hi," }
            p { "Good news — your Battlesnake \"" (snake_name) "\" is passing our health checks again and has been put back into Arena matchmaking automatically on:" }
            ul style="padding-left:22px;margin:12px 0 12px" {
                @for leaderboard in leaderboards { li style="margin:0 0 8px" { (leaderboard) } }
            }
            p { "No action needed." }
            p { "You can see its status any time on its profile page:" }
            (action_button("Open snake profile", profile_url))
            p { "— Battlesnake Arena" }
        }.into_string(),
    }
}

/// The email-recovery claim link (BS-7e38): sent to a play account's
/// address when a logged-in arena user asks to claim it without a usable
/// password. Clicking the link (while logged in as the requester) completes
/// the claim.
pub fn claim_verification(play_username: &str, verify_url: &str) -> EmailContent {
    let text = format!(
        "Hi {play_username},\n\
         \n\
         Someone signed in to the new Battlesnake Arena asked to claim the \
         play.battlesnake.com account registered to this address. If that \
         was you, open this link while signed in to finish bringing your \
         snakes and unlocks over:\n\
         \n\
         {verify_url}\n\
         \n\
         The link works once and expires in 30 minutes. It only works for \
         the arena account that requested it.\n\
         \n\
         If this wasn't you, ignore this email — nothing happens without \
         the link, and your play account stays unclaimed.\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailContent {
        subject: "Finish claiming your Battlesnake account".to_string(),
        text,
        html_body: html! {
            p { "Hi " (play_username) "," }
            p { "Someone signed in to the new Battlesnake Arena asked to claim the play.battlesnake.com account registered to this address. If that was you, open this link while signed in to finish bringing your snakes and unlocks over:" }
            (action_button("Finish claiming", verify_url))
            p { "The link works once and expires in 30 minutes. It only works for the arena account that requested it." }
            p { "If this wasn't you, ignore this email — nothing happens without the link, and your play account stays unclaimed." }
            p { "— Battlesnake Arena" }
        }.into_string(),
    }
}

/// Assemble recipient-neutral event copy with the reason and controls.
pub fn finish(
    content: EmailContent,
    to: String,
    kind: NotificationKind,
    base_url: &str,
    unsubscribe_url: Option<&str>,
) -> cja::Result<EmailMessage> {
    if kind.toggleable() != unsubscribe_url.is_some() {
        return Err(eyre!("Unsubscribe URL does not match notification kind"));
    }
    let settings_url = format!("{}/me", base_url.trim_end_matches('/'));
    let reason = kind.email_reason();
    let mut text = content.text;
    text.push_str(&format!(
        "\nWhy you got this email: {reason}\nNotification settings: {settings_url}\n"
    ));
    if let Some(url) = unsubscribe_url {
        text.push_str(&format!("Unsubscribe from {} emails: {url}\n", kind.name()));
    }
    let html = render_layout(
        &content.subject,
        &content.html_body,
        reason,
        &settings_url,
        kind,
        unsubscribe_url,
    );
    Ok(EmailMessage {
        to,
        subject: content.subject,
        text,
        html,
        list_unsubscribe: unsubscribe_url.map(|url| format!("<{url}>")),
    })
}

fn render_layout(
    subject: &str,
    body: &str,
    reason: &str,
    settings_url: &str,
    kind: NotificationKind,
    unsubscribe_url: Option<&str>,
) -> String {
    let preheader = match kind {
        NotificationKind::SnakeMatchmakingPaused => {
            "Your snake was paused while we check its server."
        }
        NotificationKind::SnakeMatchmakingResumed => "Your snake is back in matchmaking.",
        NotificationKind::PlayAccountClaimed => "Your play account was claimed on Arena.",
        NotificationKind::ClaimVerification => "Finish your play account claim within 30 minutes.",
        NotificationKind::NotificationEmailConfirmation => "Confirm this address within 24 hours.",
    };
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                style { "@media (prefers-color-scheme: dark) {.email-bg{background:#171117!important}.email-card{background:#171117!important}.email-copy{color:#F4EFEA!important}.email-link{color:#FF5C9E!important}}" }
            }
            body class="email-bg" style="margin:0;padding:0;background:#FCFBF8;color:#17141A;font-family:'Instrument Sans',Arial,Helvetica,sans-serif" {
                div style="display:none;font-size:1px;line-height:1px;max-height:0;max-width:0;opacity:0;overflow:hidden" { (preheader) }
                table role="presentation" cellpadding="0" cellspacing="0" border="0" width="100%" class="email-bg" style="background:#FCFBF8" {
                    tr { td align="center" style="padding:20px 12px" {
                        table role="presentation" cellpadding="0" cellspacing="0" border="0" width="100%" class="email-card" style="max-width:600px;background:#FCFBF8;border:1px solid #E7DFE3" {
                            tr { td style="padding:24px 24px 18px;border-top:6px solid #C11655" {
                                div class="email-copy" style="font-family:'Bricolage Grotesque',Arial,Helvetica,sans-serif;font-size:20px;font-weight:800;letter-spacing:-.5px;color:#17141A" { "Battlesnake" span class="email-link" style="color:#C11655" { " Arena" } }
                            } }
                            tr { td class="email-copy" style="padding:4px 24px 28px;color:#17141A;font-size:16px;line-height:1.55" {
                                h1 class="email-copy" style="font-family:'Bricolage Grotesque',Arial,Helvetica,sans-serif;font-size:27px;line-height:1.2;margin:0 0 24px;color:#17141A" { (subject) }
                                (PreEscaped(body))
                            } }
                            tr { td class="email-copy" style="border-top:1px solid #E7DFE3;padding:20px 24px 28px;color:#17141A;font-size:14px;line-height:1.5" {
                                p style="margin:0 0 12px" { "Why you got this email: " (reason) }
                                p style="margin:0 0 12px" { a class="email-link" href=(settings_url) style="color:#C11655;text-decoration:underline" { "Notification settings" } }
                                @if let Some(url) = unsubscribe_url {
                                    p style="margin:0" { a class="email-link" href=(url) style="color:#C11655;text-decoration:underline" { "Unsubscribe from " (kind.name()) " emails" } }
                                }
                            } }
                        }
                    } }
                }
            }
        }
    }.into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena::design_kit::AssetKind;

    fn sample(kind: NotificationKind) -> EmailContent {
        match kind {
            NotificationKind::SnakeMatchmakingPaused => matchmaking_deactivated_content(
                "Hissy",
                &[("Royale".into(), "timeout".into())],
                "https://arena.example/snake",
            ),
            NotificationKind::SnakeMatchmakingResumed => matchmaking_reactivated_content(
                "Hissy",
                &["Royale".into()],
                "https://arena.example/snake",
            ),
            NotificationKind::PlayAccountClaimed => account_claimed("player", 2, 3),
            NotificationKind::ClaimVerification => {
                claim_verification("player", "https://arena.example/claim?token=invented")
            }
            NotificationKind::NotificationEmailConfirmation => {
                notification_email_confirmation("https://arena.example/confirm?token=invented")
            }
        }
    }

    fn decode_html_entities(value: &str) -> String {
        value
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    }

    fn hrefs(html: &str) -> Vec<String> {
        html.split("href=\"")
            .skip(1)
            .map(|part| decode_html_entities(part.split('"').next().unwrap()))
            .collect()
    }

    fn check_images(html: &str) {
        for img in html.split("<img ").skip(1) {
            let tag = img.split('>').next().unwrap();
            let src = tag
                .split("src=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            let alt = tag
                .split("alt=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert!(src.starts_with("https://") && src.ends_with(".png"));
            assert!(!alt.is_empty());
        }
    }

    #[test]
    fn all_kinds_have_matching_safe_alternatives() {
        for kind in NotificationKind::ALL {
            let unsubscribe = kind
                .toggleable()
                .then_some("https://arena.example/unsubscribe");
            let message = finish(
                sample(kind),
                "test@example.com".into(),
                kind,
                "https://arena.example",
                unsubscribe,
            )
            .unwrap();
            assert!(!message.text.is_empty() && !message.html.is_empty());
            assert!(message.text.contains(kind.email_reason()));
            assert!(message.html.contains(kind.email_reason()));
            assert!(message.text.contains("https://arena.example/me"));
            assert!(message.html.contains("https://arena.example/me"));
            let header = message.html.split("<h1").next().unwrap();
            assert!(
                !header.contains("<a "),
                "wordmark header must not contain a link"
            );
            for url in hrefs(&message.html) {
                assert!(message.text.contains(&url), "missing {url} in text");
            }
            assert_eq!(
                message.html.matches("class=\"email-cta\"").count(),
                usize::from(kind != NotificationKind::PlayAccountClaimed)
            );
            assert_eq!(
                message.html.matches("/unsubscribe\"").count(),
                usize::from(kind.toggleable())
            );
            assert_eq!(message.list_unsubscribe.is_some(), kind.toggleable());
            for forbidden in [
                "<script",
                "<svg",
                "<link",
                "@import",
                "@font-face",
                "fonts.googleapis",
            ] {
                assert!(!message.html.contains(forbidden), "{forbidden}");
            }
            check_images(&message.html);
            if kind == NotificationKind::NotificationEmailConfirmation {
                assert_eq!(
                    message.subject,
                    "Confirm your Battlesnake notification email"
                );
                assert!(message.html.contains(&message.subject));
                for phrase in [
                    "Confirm this address for optional Battlesnake Arena emails within 24 hours:",
                    "https://arena.example/confirm?token=invented",
                    "Sign in to the same Arena account that requested this change.",
                    "If your email app opens a signed-out browser, sign in and reopen this link.",
                    "If you did not request this, you can ignore this email.",
                ] {
                    assert!(message.text.contains(phrase), "text missing {phrase}");
                    assert!(message.html.contains(phrase), "HTML missing {phrase}");
                }
            }
        }
    }

    #[test]
    fn event_fragments_preserve_copy_and_explicit_links() {
        let cases = [
            (
                sample(NotificationKind::SnakeMatchmakingPaused),
                &[
                    "Your Battlesnake",
                    "has been temporarily removed",
                    "We test each leaderboard",
                    "Its ratings are safe",
                    "We'll keep checking",
                    "The &quot;Test Snake&quot; button",
                    "https://arena.example/snake",
                ][..],
                true,
            ),
            (
                sample(NotificationKind::SnakeMatchmakingResumed),
                &[
                    "Good news",
                    "is passing our health checks again",
                    "No action needed.",
                    "You can see its status",
                    "https://arena.example/snake",
                ][..],
                true,
            ),
            (
                sample(NotificationKind::PlayAccountClaimed),
                &[
                    "Your play.battlesnake.com account was just claimed",
                    "We moved",
                    "If this was you",
                    "If this WASN'T you",
                    "https://arena.battlesnake.com/discord",
                ][..],
                false,
            ),
            (
                sample(NotificationKind::ClaimVerification),
                &[
                    "Someone signed in",
                    "The link works once and expires in 30 minutes.",
                    "It only works for the arena account",
                    "If this wasn't you",
                    "https://arena.example/claim?token=invented",
                ][..],
                true,
            ),
            (
                sample(NotificationKind::NotificationEmailConfirmation),
                &[
                    "Confirm this address for optional",
                    "Sign in to the same Arena account",
                    "If your email app opens a signed-out browser",
                    "If you did not request this",
                    "https://arena.example/confirm?token=invented",
                ][..],
                true,
            ),
        ];
        for (content, phrases, has_cta) in cases {
            for phrase in phrases {
                assert!(content.html_body.contains(phrase), "HTML missing {phrase}");
            }
            assert_eq!(
                content.html_body.matches("class=\"email-cta\"").count(),
                usize::from(has_cta)
            );
            for url in hrefs(&content.html_body) {
                assert!(content.text.contains(&url));
            }
        }
        for kind in [
            NotificationKind::SnakeMatchmakingPaused,
            NotificationKind::SnakeMatchmakingResumed,
        ] {
            let html = sample(kind).html_body;
            assert!(html.contains("<ul "));
            assert!(html.contains("<li "));
        }
    }

    #[test]
    fn user_supplied_url_like_lines_are_never_links() {
        let evil = "x\nhttps://evil.example";
        let old_text = matchmaking_deactivated_content(
            evil,
            &[(evil.into(), evil.into())],
            "https://arena.example/snake",
        )
        .text;
        let old_html = EmailContent::from_text("old".into(), old_text).html_body;
        assert!(
            hrefs(&old_html)
                .iter()
                .any(|url| url.contains("evil.example")),
            "legacy transcription must exercise the old defect"
        );
        for content in [
            matchmaking_deactivated_content(
                evil,
                &[(evil.into(), evil.into())],
                "https://arena.example/snake",
            ),
            matchmaking_reactivated_content(evil, &[evil.into()], "https://arena.example/snake"),
        ] {
            assert!(content.html_body.contains("https://evil.example"));
            assert!(
                hrefs(&content.html_body)
                    .iter()
                    .all(|url| !url.contains("evil.example"))
            );
        }
    }

    #[test]
    fn ampersand_link_parity_requires_entity_decoding() {
        let url = "https://arena.example/claim?a=1&b=2";
        let content = claim_verification("player", url);
        assert!(
            content
                .html_body
                .contains("href=\"https://arena.example/claim?a=1&amp;b=2\"")
        );
        assert!(
            !content
                .text
                .contains("https://arena.example/claim?a=1&amp;b=2")
        );
        assert!(
            hrefs(&content.html_body)
                .iter()
                .all(|href| content.text.contains(href))
        );
    }

    #[test]
    fn user_strings_are_escaped() {
        let evil = "<script>\" &";
        for content in [
            account_claimed(evil, 1, 1),
            claim_verification(evil, "https://arena.example/claim"),
            matchmaking_deactivated_content(
                evil,
                &[(evil.into(), evil.into())],
                "https://arena.example/snake",
            ),
            matchmaking_reactivated_content(evil, &[evil.into()], "https://arena.example/snake"),
        ] {
            assert!(!content.html_body.contains("<script>"));
            assert!(content.html_body.contains("&lt;script&gt;"));
        }
    }

    #[test]
    fn item_images_are_https_pngs() {
        let head = crate::routes::og::customization_png_url(AssetKind::Head, "default").unwrap();
        let tail = crate::routes::og::customization_png_url(AssetKind::Tail, "default").unwrap();
        let fixture =
            html! { img src=(head) alt="Default head"; img src=(tail) alt="Default tail"; }
                .into_string();
        assert_eq!(fixture.matches("<img ").count(), 2);
        check_images(&fixture);
        assert!(crate::routes::og::customization_png_url(AssetKind::Head, "unknown").is_none());
    }

    #[test]
    fn unsubscribe_url_must_match_kind() {
        assert!(
            finish(
                sample(NotificationKind::SnakeMatchmakingPaused),
                "a@b".into(),
                NotificationKind::SnakeMatchmakingPaused,
                "https://arena.example",
                None
            )
            .is_err()
        );
        assert!(
            finish(
                sample(NotificationKind::PlayAccountClaimed),
                "a@b".into(),
                NotificationKind::PlayAccountClaimed,
                "https://arena.example",
                Some("https://arena.example/unsubscribe")
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "writes sanitized review previews under /tmp"]
    fn write_review_previews() {
        let dir = std::path::Path::new("/tmp/arena-DEV-1675-previews");
        std::fs::create_dir_all(dir).unwrap();
        for kind in NotificationKind::ALL {
            let unsubscribe = kind
                .toggleable()
                .then_some("https://arena.example/unsubscribe?token=invented");
            let message = finish(
                sample(kind),
                "preview@example.com".into(),
                kind,
                "https://arena.example",
                unsubscribe,
            )
            .unwrap();
            std::fs::write(dir.join(format!("{}.html", kind.key())), message.html).unwrap();
        }
    }

    #[test]
    fn matchmaking_deactivated_renders_snake_and_recovery_link() {
        let EmailContent { subject, text, .. } = matchmaking_deactivated_content(
            "Hissy",
            &[(
                "Royale 11x11".to_string(),
                "POST /move (turn 0): Returned non-success HTTP status 500".to_string(),
            )],
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert_eq!(subject, "Hissy was paused from Arena matchmaking");
        assert!(text.contains("\"Hissy\""));
        assert!(text.contains("on this Arena leaderboard because"));
        assert!(text.contains(
            "- Royale 11x11: POST /move (turn 0): Returned non-success HTTP status 500\n"
        ));
        assert!(text.contains("https://arena.example.com/battlesnakes/abc/profile"));
    }

    #[test]
    fn matchmaking_deactivated_lists_every_pulled_leaderboard() {
        let EmailContent { text, .. } = matchmaking_deactivated_content(
            "Hissy",
            &[
                ("Royale 11x11".to_string(), "boom".to_string()),
                ("Constrictor 11x11".to_string(), "bang".to_string()),
            ],
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert!(text.contains("on these Arena leaderboards because"));
        assert!(text.contains("- Royale 11x11: boom\n- Constrictor 11x11: bang\n"));
    }

    #[test]
    fn matchmaking_reactivated_renders_snake_and_profile_link() {
        let EmailContent { subject, text, .. } = matchmaking_reactivated_content(
            "Hissy",
            &["Royale 11x11".to_string(), "Duels 11x11".to_string()],
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert_eq!(subject, "Hissy is back in Arena matchmaking");
        assert!(text.contains("\"Hissy\""));
        assert!(text.contains("automatically on: Royale 11x11, Duels 11x11."));
        assert!(text.contains("https://arena.example.com/battlesnakes/abc/profile"));
    }

    #[test]
    fn claim_verification_renders_link_and_expiry_note() {
        let msg = claim_verification(
            "coolsnake",
            "https://arena.example.com/claim/email/verify?token=abc123",
        );
        assert_eq!(msg.subject, "Finish claiming your Battlesnake account");
        assert!(msg.text.contains("Hi coolsnake,"));
        assert!(
            msg.text
                .contains("https://arena.example.com/claim/email/verify?token=abc123")
        );
        assert!(msg.text.contains("expires in 30 minutes"));
        assert!(msg.text.contains("ignore this email"));
    }

    #[test]
    fn account_claimed_renders_counts_and_recipient() {
        let msg = account_claimed("coolsnake", 3, 2);
        assert_eq!(msg.subject, "Your Battlesnake account was claimed");
        assert!(msg.text.contains("Hi coolsnake,"));
        assert!(msg.text.contains("3 snake(s)"));
        assert!(msg.text.contains("2 customization unlock(s)"));
        assert!(msg.text.contains("Discord"));
    }

    #[test]
    fn account_claimed_handles_singular_zero_counts() {
        let msg = account_claimed("solo", 0, 0);
        assert!(msg.text.contains("0 snake(s)"));
        assert!(msg.text.contains("0 customization unlock(s)"));
    }
}
