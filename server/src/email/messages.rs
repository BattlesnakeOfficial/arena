//! Typed builders for the plain-text emails arena sends. Each function
//! returns a fully-rendered [`EmailMessage`] so send sites stay declarative
//! and the copy lives in one place. New flows (deactivation notices,
//! pre-cutover announcements) add a builder here.

/// A rendered, ready-to-send message. Plain text only for now — arena has no
/// HTML templating and transactional copy stays legible without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    pub to: String,
    pub subject: String,
    pub text: String,
}

/// Sent to a play account's email when that account is claimed on arena
/// (via GitHub auto-link or password claim). Doubles as a security notice:
/// the real owner hears about a claim even if someone else initiated it.
pub fn account_claimed(
    to_email: &str,
    play_username: &str,
    snakes_migrated: usize,
    grants_migrated: u64,
) -> EmailMessage {
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
         we can secure your account.\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailMessage {
        to: to_email.to_string(),
        subject: "Your Battlesnake account was claimed".to_string(),
        text,
    }
}

/// Sent to a snake's owner when the health sweeper pulls the snake from
/// leaderboard matchmaking after repeated failed probes. Mirrors play's
/// arena_matchmaking_deactivated notice: says what happened, why, and how to
/// get back in.
pub fn matchmaking_deactivated(
    to_email: &str,
    snake_name: &str,
    failure_summary: &str,
    profile_url: &str,
) -> EmailMessage {
    let text = format!(
        "Hi,\n\
         \n\
         Your Battlesnake \"{snake_name}\" has been temporarily removed from \
         Arena leaderboard matchmaking due to repeated timeouts or errors \
         from its server.\n\
         \n\
         Most recent problem: {failure_summary}\n\
         \n\
         Its ratings are safe — we stop matching it so a down server doesn't \
         hurt its standing. Once your snake is fixed, resume matchmaking from \
         its profile page:\n\
         \n\
         {profile_url}\n\
         \n\
         (The \"Test Snake\" button there runs the same checks we do.)\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailMessage {
        to: to_email.to_string(),
        subject: format!("{snake_name} was paused from Arena matchmaking"),
        text,
    }
}

/// Sent when the health sweeper puts a previously deactivated snake back
/// into matchmaking on its own, after enough consecutive healthy probes.
/// Closes the loop on [`matchmaking_deactivated`] so the owner isn't left
/// thinking they still need to press Resume.
pub fn matchmaking_reactivated(
    to_email: &str,
    snake_name: &str,
    profile_url: &str,
) -> EmailMessage {
    let text = format!(
        "Hi,\n\
         \n\
         Good news — your Battlesnake \"{snake_name}\" is responding again \
         and has been put back into Arena leaderboard matchmaking \
         automatically. No action needed.\n\
         \n\
         You can see its status any time on its profile page:\n\
         \n\
         {profile_url}\n\
         \n\
         — Battlesnake Arena\n"
    );

    EmailMessage {
        to: to_email.to_string(),
        subject: format!("{snake_name} is back in Arena matchmaking"),
        text,
    }
}

/// The email-recovery claim link (BS-7e38): sent to a play account's
/// address when a logged-in arena user asks to claim it without a usable
/// password. Clicking the link (while logged in as the requester) completes
/// the claim.
pub fn claim_verification(to_email: &str, play_username: &str, verify_url: &str) -> EmailMessage {
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

    EmailMessage {
        to: to_email.to_string(),
        subject: "Finish claiming your Battlesnake account".to_string(),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matchmaking_deactivated_renders_snake_and_recovery_link() {
        let msg = matchmaking_deactivated(
            "owner@example.com",
            "Hissy",
            "POST /move: request timed out",
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert_eq!(msg.to, "owner@example.com");
        assert_eq!(msg.subject, "Hissy was paused from Arena matchmaking");
        assert!(msg.text.contains("\"Hissy\""));
        assert!(msg.text.contains("POST /move: request timed out"));
        assert!(
            msg.text
                .contains("https://arena.example.com/battlesnakes/abc/profile")
        );
    }

    #[test]
    fn matchmaking_reactivated_renders_snake_and_profile_link() {
        let msg = matchmaking_reactivated(
            "owner@example.com",
            "Hissy",
            "https://arena.example.com/battlesnakes/abc/profile",
        );
        assert_eq!(msg.to, "owner@example.com");
        assert_eq!(msg.subject, "Hissy is back in Arena matchmaking");
        assert!(msg.text.contains("\"Hissy\""));
        assert!(msg.text.contains("automatically"));
        assert!(
            msg.text
                .contains("https://arena.example.com/battlesnakes/abc/profile")
        );
    }

    #[test]
    fn claim_verification_renders_link_and_expiry_note() {
        let msg = claim_verification(
            "player@example.com",
            "coolsnake",
            "https://arena.example.com/claim/email/verify?token=abc123",
        );
        assert_eq!(msg.to, "player@example.com");
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
        let msg = account_claimed("player@example.com", "coolsnake", 3, 2);
        assert_eq!(msg.to, "player@example.com");
        assert_eq!(msg.subject, "Your Battlesnake account was claimed");
        assert!(msg.text.contains("Hi coolsnake,"));
        assert!(msg.text.contains("3 snake(s)"));
        assert!(msg.text.contains("2 customization unlock(s)"));
        assert!(msg.text.contains("Discord"));
    }

    #[test]
    fn account_claimed_handles_singular_zero_counts() {
        let msg = account_claimed("p@example.com", "solo", 0, 0);
        assert!(msg.text.contains("0 snake(s)"));
        assert!(msg.text.contains("0 customization unlock(s)"));
    }
}
