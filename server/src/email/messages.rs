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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockedItem {
    pub display_name: String,
    pub slug: String,
    pub png_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AchievementUnlock {
    pub name: String,
    pub head: Option<UnlockedItem>,
    pub tail: Option<UnlockedItem>,
}

pub fn achievement_earned(
    achievements: &[AchievementUnlock],
    achievements_url: &str,
) -> EmailContent {
    let subject = if achievements.len() == 1 {
        format!("Achievement unlocked: {}", achievements[0].name)
    } else {
        format!("{} achievements unlocked", achievements.len())
    };
    let mut text = format!("{subject}\n\n");
    for achievement in achievements {
        text.push_str(&format!("{}\n", achievement.name));
        for (kind, item) in [("head", &achievement.head), ("tail", &achievement.tail)] {
            if let Some(item) = item {
                text.push_str(&format!(
                    "Unlocked: {} {} — set {} to \"{}\" in your snake's info response.\n",
                    if kind == "head" { "Head" } else { "Tail" },
                    item.display_name,
                    kind,
                    item.slug,
                ));
            }
        }
        text.push('\n');
    }
    text.push_str(&format!("View achievements: {achievements_url}\n"));
    let html_body = html! {
        @for achievement in achievements {
            h2 style="font-size:20px;margin:24px 0 12px" { (&achievement.name) }
            @for (kind, item) in [("head", &achievement.head), ("tail", &achievement.tail)] {
                @if let Some(item) = item {
                    p { "Unlocked: " (if kind == "head" { "Head" } else { "Tail" }) " " (&item.display_name) }
                    p { img src=(&item.png_url) alt=(format!("{} {} unlocked by {}", item.display_name, kind, achievement.name)) width="96" height="96"; }
                    p { "Set " (kind) " to \"" (&item.slug) "\" in your snake's info response." }
                }
            }
        }
        (action_button("View achievements", achievements_url))
    }.into_string();
    EmailContent {
        subject,
        text,
        html_body,
    }
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

pub(crate) fn action_button(label: &str, url: &str) -> Markup {
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
            "Confirm this address for optional Battlesnake emails within 24 hours:\n\n{url}\n\nSign in to the same Battlesnake account that requested this change. If your email app opens a signed-out browser, sign in and reopen this link. If you did not request this, you can ignore this email.\n"
        ),
        html_body: html! {
            p { "Confirm this address for optional Battlesnake emails within 24 hours:" }
            (action_button("Confirm this address", url))
            p { "Sign in to the same Battlesnake account that requested this change. If your email app opens a signed-out browser, sign in and reopen this link. If you did not request this, you can ignore this email." }
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
         Battlesnake. We moved {snakes_migrated} snake(s) and \
         {grants_migrated} customization unlock(s) over to your account.\n\
         \n\
         If this was you, there's nothing to do — welcome back!\n\
         \n\
         If this WASN'T you, please reach out to us on Discord right away so \
         we can secure your account:\n\
         \n\
         https://arena.battlesnake.com/discord\n\
         \n\
         — Battlesnake\n"
    );

    EmailContent {
        subject: "Your Battlesnake account was claimed".to_string(),
        text,
        html_body: html! {
            p { "Hi " (play_username) "," }
            p { "Your play.battlesnake.com account was just claimed on the new Battlesnake. We moved " (snakes_migrated) " snake(s) and " (grants_migrated) " customization unlock(s) over to your account." }
            p { "If this was you, there's nothing to do — welcome back!" }
            p { "If this WASN'T you, please reach out to us on Discord right away so we can secure your account:" }
            p { a class="email-link" href="https://arena.battlesnake.com/discord" style="color:#C11655;text-decoration:underline" { "https://arena.battlesnake.com/discord" } }
            p { "— Battlesnake" }
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
        "this leaderboard"
    } else {
        "these leaderboards"
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
         — Battlesnake\n"
    );

    EmailContent {
        subject: format!("{snake_name} was paused from matchmaking"),
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
            p { "— Battlesnake" }
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
         checks again and has been put back into matchmaking \
         automatically on: {leaderboards_text}. No action needed.\n\
         \n\
         You can see its status any time on its profile page:\n\
         \n\
         {profile_url}\n\
         \n\
         — Battlesnake\n"
    );

    EmailContent {
        subject: format!("{snake_name} is back in matchmaking"),
        text,
        html_body: html! {
            p { "Hi," }
            p { "Good news — your Battlesnake \"" (snake_name) "\" is passing our health checks again and has been put back into matchmaking automatically on:" }
            ul style="padding-left:22px;margin:12px 0 12px" {
                @for leaderboard in leaderboards { li style="margin:0 0 8px" { (leaderboard) } }
            }
            p { "No action needed." }
            p { "You can see its status any time on its profile page:" }
            (action_button("Open snake profile", profile_url))
            p { "— Battlesnake" }
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
         Someone signed in to Battlesnake asked to claim the \
         play.battlesnake.com account registered to this address. If that \
         was you, open this link while signed in to finish bringing your \
         snakes and unlocks over:\n\
         \n\
         {verify_url}\n\
         \n\
         The link works once and expires in 30 minutes. It only works for \
         the Battlesnake account that requested it.\n\
         \n\
         If this wasn't you, ignore this email — nothing happens without \
         the link, and your play account stays unclaimed.\n\
         \n\
         — Battlesnake\n"
    );

    EmailContent {
        subject: "Finish claiming your Battlesnake account".to_string(),
        text,
        html_body: html! {
            p { "Hi " (play_username) "," }
            p { "Someone signed in to Battlesnake asked to claim the play.battlesnake.com account registered to this address. If that was you, open this link while signed in to finish bringing your snakes and unlocks over:" }
            (action_button("Finish claiming", verify_url))
            p { "The link works once and expires in 30 minutes. It only works for the Battlesnake account that requested it." }
            p { "If this wasn't you, ignore this email — nothing happens without the link, and your play account stays unclaimed." }
            p { "— Battlesnake" }
        }.into_string(),
    }
}

/// Inert examples built with the same content functions as delivery jobs.
/// An exhaustive match makes a new kind require an explicit sample decision.
pub fn sample_notification(kind: NotificationKind, base_url: &str) -> Option<EmailContent> {
    let base = base_url.trim_end_matches('/');
    let profile = format!("{base}/battlesnakes/00000000-0000-4000-8000-000000000001/profile");
    match kind {
        NotificationKind::SnakeMatchmakingPaused => Some(matchmaking_deactivated_content(
            "Sample Snake",
            &[
                ("Royale 11x11".into(), "POST /move timed out".into()),
                ("Duels 11x11".into(), "HTTP 500".into()),
            ],
            &profile,
        )),
        NotificationKind::SnakeMatchmakingResumed => Some(matchmaking_reactivated_content(
            "Sample Snake",
            &["Royale 11x11".into(), "Duels 11x11".into()],
            &profile,
        )),
        NotificationKind::PlayAccountClaimed => Some(account_claimed("sample-player", 3, 2)),
        NotificationKind::ClaimVerification => Some(claim_verification(
            "sample-player",
            &format!("{base}/claim/email/verify?token=sample-invalid-token"),
        )),
        NotificationKind::NotificationEmailConfirmation => Some(notification_email_confirmation(
            &format!("{base}/settings/notification-email/confirm?token=sample-invalid-token"),
        )),
        NotificationKind::AchievementEarned => Some(achievement_earned(
            &[
                crate::customizations::achievements::achievement_unlock(
                    crate::customizations::achievements::Achievement::FirstWin,
                    true,
                    false,
                )
                .ok()?,
                crate::customizations::achievements::achievement_unlock(
                    crate::customizations::achievements::Achievement::CrowdPleaser,
                    true,
                    true,
                )
                .ok()?,
            ],
            &format!("{base}/customizations/achievements"),
        )),
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
    let footer = html! {
        p style="margin:0 0 12px" { "Why you got this email: " (reason) }
        p style="margin:0 0 12px" { a class="email-link" href=(settings_url) style="color:#C11655;text-decoration:underline" { "Notification settings" } }
        @if let Some(url) = unsubscribe_url {
            p style="margin:0" { a class="email-link" href=(url) style="color:#C11655;text-decoration:underline" { "Unsubscribe from " (kind.name()) " emails" } }
        }
    };
    let html = render_layout(
        &content.subject,
        notification_preheader(kind),
        &content.html_body,
        footer,
    );
    Ok(EmailMessage {
        to,
        subject: content.subject,
        text,
        html,
        list_unsubscribe: unsubscribe_url.map(|url| format!("<{url}>")),
    })
}

fn notification_preheader(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::SnakeMatchmakingPaused => {
            "Your snake was paused while we check its server."
        }
        NotificationKind::SnakeMatchmakingResumed => "Your snake is back in matchmaking.",
        NotificationKind::PlayAccountClaimed => "Your play account was claimed on Battlesnake.",
        NotificationKind::ClaimVerification => "Finish your play account claim within 30 minutes.",
        NotificationKind::NotificationEmailConfirmation => "Confirm this address within 24 hours.",
        NotificationKind::AchievementEarned => "You unlocked new snake cosmetics.",
    }
}

/// The branded shell shared by every email: header, `subject` as the title,
/// the body, and a footer that says why the reader got it.
pub(crate) fn render_layout(subject: &str, preheader: &str, body: &str, footer: Markup) -> String {
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
                                div class="email-copy" style="font-family:'Bricolage Grotesque',Arial,Helvetica,sans-serif;font-size:20px;font-weight:800;letter-spacing:-.5px;color:#17141A" { "Battlesnake" }
                            } }
                            tr { td class="email-copy" style="padding:4px 24px 28px;color:#17141A;font-size:16px;line-height:1.55" {
                                h1 class="email-copy" style="font-family:'Bricolage Grotesque',Arial,Helvetica,sans-serif;font-size:27px;line-height:1.2;margin:0 0 24px;color:#17141A" { (subject) }
                                (PreEscaped(body))
                            } }
                            tr { td class="email-copy" style="border-top:1px solid #E7DFE3;padding:20px 24px 28px;color:#17141A;font-size:14px;line-height:1.5" {
                                (footer)
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

    #[test]
    fn achievement_email_has_images_slugs_and_matching_alternatives() {
        use crate::customizations::achievements::{Achievement, achievement_unlock};
        let first = achievement_unlock(Achievement::FirstWin, true, false).unwrap();
        let crowd = achievement_unlock(Achievement::CrowdPleaser, true, true).unwrap();
        let url = "https://arena.example/customizations/achievements";
        let singular = achievement_earned(std::slice::from_ref(&first), url);
        assert_eq!(singular.subject, "Achievement unlocked: First Win");
        assert!(singular.text.contains("set head to \"frog\""));
        assert!(singular.html_body.contains("frog.png"));
        let plural = achievement_earned(&[first, crowd], url);
        assert_eq!(plural.subject, "2 achievements unlocked");
        for value in ["First Win", "Crowd Pleaser", "frog", "monkey", url] {
            assert!(plural.text.contains(value));
            assert!(plural.html_body.contains(value));
        }
        assert_eq!(plural.html_body.matches("<img ").count(), 3);
        check_images(&plural.html_body);
        assert!(plural.text.contains("set tail to \"monkey\""));
        for forbidden in ["2024", "Collection", "purchase", "buy"] {
            assert!(!plural.text.contains(forbidden));
            assert!(!plural.html_body.contains(forbidden));
        }
    }

    #[test]
    fn every_email_kind_has_one_nonempty_sample() {
        use crate::notifications::NotificationChannel;
        let mut keys = std::collections::HashSet::new();
        for kind in NotificationKind::ALL {
            assert!(keys.insert(kind.key()), "duplicate key: {}", kind.key());
            let sample = sample_notification(kind, "https://arena.example/");
            assert_eq!(sample.is_some(), kind.supports(NotificationChannel::Email));
            if let Some(sample) = sample {
                assert!(!sample.subject.is_empty());
                assert!(!sample.text.is_empty());
                assert!(!sample.html_body.is_empty());
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
            assert!(!alt.is_empty());
            let url = url::Url::parse(src).expect("image URL must be absolute");
            assert_eq!(url.scheme(), "https");
            let parts: Vec<_> = url.path_segments().unwrap().collect();
            assert_eq!(parts.len(), 4);
            assert_eq!(parts[0], "og");
            assert_eq!(parts[1], "customizations");
            let kind = match parts[2] {
                "heads" => AssetKind::Head,
                "tails" => AssetKind::Tail,
                _ => panic!("unknown customization image kind"),
            };
            let slug = parts[3].strip_suffix(".png").expect("image must be PNG");
            assert_eq!(
                Some(src.to_string()),
                crate::routes::og::customization_png_url(kind, slug)
            );
        }
    }

    #[test]
    fn all_kinds_have_matching_safe_alternatives() {
        for kind in NotificationKind::ALL {
            let unsubscribe = kind
                .toggleable()
                .then_some("https://arena.example/unsubscribe");
            let message = finish(
                sample_notification(kind, "https://arena.example").unwrap(),
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
            // DEV-1751: the product is branded Battlesnake — no "Arena" may
            // survive in any rendered email surface. Case-sensitive on purpose:
            // lowercase "arena" legitimately appears in test URLs like
            // https://arena.example.
            assert!(
                !message.subject.contains("Arena"),
                "{}: subject mentions Arena",
                kind.key()
            );
            for body in [&message.text, &message.html] {
                assert!(
                    !body.contains("Arena"),
                    "{}: rendered email mentions Arena",
                    kind.key()
                );
            }
            if kind == NotificationKind::NotificationEmailConfirmation {
                assert_eq!(
                    message.subject,
                    "Confirm your Battlesnake notification email"
                );
                assert!(message.html.contains(&message.subject));
                for phrase in [
                    "Confirm this address for optional Battlesnake emails within 24 hours:",
                    "https://arena.example/settings/notification-email/confirm?token=sample-invalid-token",
                    "Sign in to the same Battlesnake account that requested this change.",
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
                sample_notification(
                    NotificationKind::SnakeMatchmakingPaused,
                    "https://arena.example",
                )
                .unwrap(),
                &[
                    "Your Battlesnake",
                    "has been temporarily removed",
                    "We test each leaderboard",
                    "Its ratings are safe",
                    "We'll keep checking",
                    "The &quot;Test Snake&quot; button",
                    "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
                ][..],
                true,
            ),
            (
                sample_notification(
                    NotificationKind::SnakeMatchmakingResumed,
                    "https://arena.example",
                )
                .unwrap(),
                &[
                    "Good news",
                    "is passing our health checks again",
                    "No action needed.",
                    "You can see its status",
                    "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
                ][..],
                true,
            ),
            (
                sample_notification(
                    NotificationKind::PlayAccountClaimed,
                    "https://arena.example",
                )
                .unwrap(),
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
                sample_notification(NotificationKind::ClaimVerification, "https://arena.example")
                    .unwrap(),
                &[
                    "Someone signed in",
                    "The link works once and expires in 30 minutes.",
                    "It only works for the Battlesnake account",
                    "If this wasn't you",
                    "https://arena.example/claim/email/verify?token=sample-invalid-token",
                ][..],
                true,
            ),
            (
                sample_notification(
                    NotificationKind::NotificationEmailConfirmation,
                    "https://arena.example",
                )
                .unwrap(),
                &[
                    "Confirm this address for optional",
                    "Sign in to the same Battlesnake account",
                    "If your email app opens a signed-out browser",
                    "If you did not request this",
                    "https://arena.example/settings/notification-email/confirm?token=sample-invalid-token",
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
            let html = sample_notification(kind, "https://arena.example")
                .unwrap()
                .html_body;
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
            "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
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
                "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
            ),
            matchmaking_reactivated_content(
                evil,
                &[evil.into()],
                "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
            ),
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
                "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
            ),
            matchmaking_reactivated_content(
                evil,
                &[evil.into()],
                "https://arena.example/battlesnakes/00000000-0000-4000-8000-000000000001/profile",
            ),
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
        for bad in [
            "<img src=\"https://arena.battlesnake.com/og/customizations/heads/default.svg\" alt=\"Head\">",
            "<img src=\"https://arena.battlesnake.com/og/customizations/heads/unknown.png\" alt=\"Head\">",
            "<img src=\"https://arena.battlesnake.com/og/customizations/heads/default.png\" alt=\"\">",
            "<img src=\"/og/customizations/heads/default.png\" alt=\"Head\">",
        ] {
            assert!(std::panic::catch_unwind(|| check_images(bad)).is_err());
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sample_image_fixture_resolves_through_production_router(pool: sqlx::PgPool) {
        use axum::{
            body::{Body, to_bytes},
            http::{Request, StatusCode, header},
        };
        use tower::ServiceExt as _;
        let app = crate::routes::routes(crate::state::AppState::test_from_pool(pool))
            .expect("asset router")
            .into_router();
        let head = crate::routes::og::customization_png_url(AssetKind::Head, "default").unwrap();
        let tail = crate::routes::og::customization_png_url(AssetKind::Tail, "default").unwrap();
        let fixture =
            html! { img src=(head) alt="Default head"; img src=(tail) alt="Default tail"; }
                .into_string();
        assert_eq!(fixture.matches("<img ").count(), 2);
        let mut rendered = vec![fixture];
        for kind in NotificationKind::ALL {
            let Some(sample) = sample_notification(kind, "https://arena.example") else {
                continue;
            };
            let unsubscribe = kind
                .toggleable()
                .then_some("https://arena.example/notifications/unsubscribe/sample-invalid-token");
            let message = finish(
                sample,
                "sample@example.com".into(),
                kind,
                "https://arena.example",
                unsubscribe,
            )
            .unwrap();
            rendered.push(message.html);
        }
        for html in rendered {
            check_images(&html);
            for img in html.split("<img ").skip(1) {
                let url = img
                    .split('>')
                    .next()
                    .unwrap()
                    .split("src=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap();
                let path = url::Url::parse(url).unwrap().path().to_owned();
                let response = app
                    .clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers().get(header::CONTENT_TYPE).unwrap(),
                    "image/png"
                );
                let png = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
        }
    }

    #[test]
    fn unsubscribe_url_must_match_kind() {
        assert!(
            finish(
                sample_notification(
                    NotificationKind::SnakeMatchmakingPaused,
                    "https://arena.example"
                )
                .unwrap(),
                "a@b".into(),
                NotificationKind::SnakeMatchmakingPaused,
                "https://arena.example",
                None
            )
            .is_err()
        );
        assert!(
            finish(
                sample_notification(
                    NotificationKind::PlayAccountClaimed,
                    "https://arena.example"
                )
                .unwrap(),
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
                sample_notification(kind, "https://arena.example").unwrap(),
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
        assert_eq!(subject, "Hissy was paused from matchmaking");
        assert!(text.contains("\"Hissy\""));
        assert!(text.contains("on this leaderboard because"));
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
        assert!(text.contains("on these leaderboards because"));
        assert!(text.contains("- Royale 11x11: boom\n- Constrictor 11x11: bang\n"));
    }

    #[test]
    fn matchmaking_reactivated_renders_snake_and_profile_link() {
        let EmailContent { subject, text, .. } = matchmaking_reactivated_content(
            "Hissy",
            &["Royale 11x11".to_string(), "Duels 11x11".to_string()],
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert_eq!(subject, "Hissy is back in matchmaking");
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
