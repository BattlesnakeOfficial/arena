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

#[cfg(test)]
mod tests {
    use super::*;

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
