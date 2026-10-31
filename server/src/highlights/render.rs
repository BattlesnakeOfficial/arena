//! Renderers for the highlights digest: markdown (CLI parity run), HTML
//! (admin pages), the store-time Discord snapshot, and the frames-missing
//! banner.

use maud::{Markup, html};
use uuid::Uuid;

use super::types::{ComputedDigest, DiscordPicks};

/// Thousands-grouped integer, like Python's `{:,}` (Rust's format!
/// has no grouping option).
fn thousands(n: i64) -> String {
    let raw = n.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// The `game · card` link pair every listed game carries.
fn links_markdown(base_url: &str, game_id: Uuid) -> String {
    format!(
        "[game]({base}/games/{game_id}) · [card]({base}/og/games/{game_id}.png?showSpoilers=true)",
        base = base_url.trim_end_matches('/')
    )
}

/// Render a computed digest as markdown, mirroring the prototype's
/// `digests/DATE.md`. Used by the `highlights-preview` CLI.
pub fn render_markdown(digest: &ComputedDigest, base_url: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Arena highlights: {} to {}\n\n",
        digest.window_start.format("%a %b %d %H:%MZ"),
        digest.window_end.format("%a %b %d %H:%MZ"),
    ));
    let counts = digest
        .per_ladder_counts
        .iter()
        .map(|(name, count)| {
            format!(
                "{} {}",
                name.split(' ').next().unwrap_or(name),
                thousands(*count)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "{} rated ladder games: {counts}. Candidates only, for a human to pick \
         from. Links go to the replay and its spoiler card. House snakes are \
         tagged and never headlined.\n",
        thousands(digest.games_count)
    ));

    let mut last_kind = None;
    for section in &digest.sections {
        if Some(section.kind) != last_kind {
            out.push_str(&format!("\n## {}\n\n", section.kind.heading()));
            last_kind = Some(section.kind);
        }
        match &section.ladder {
            Some(ladder) => {
                out.push_str(&format!("- **{}**: {}", ladder.name, section.summary));
                for item in &section.items {
                    out.push_str(&format!("\n  - {}", item.text));
                    if let Some(game_id) = item.game_id {
                        out.push_str(&format!(" {}", links_markdown(base_url, game_id)));
                    }
                }
                out.push('\n');
            }
            None => {
                if section.items.is_empty() {
                    out.push_str(&format!("- {}\n", section.summary));
                }
                for item in &section.items {
                    out.push_str(&format!("- {}", item.text));
                    if let Some(game_id) = item.game_id {
                        out.push_str(&format!(" {}", links_markdown(base_url, game_id)));
                    }
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Render stored snapshot text for the admin pages. Item/summary text is
/// markdown-flavored (`**name**`, for the Discord snapshot and the CLI
/// digest); the HTML view converts those bold pairs to `<strong>` while
/// maud-escaping every segment (snake names are user-controlled). An
/// unmatched trailing `**` renders literally, in place (before the
/// segment it preceded in the source text).
fn text_html(text: &str) -> Markup {
    let parts: Vec<&str> = text.split("**").collect();
    // Odd marker count: the final marker is unpaired — it renders as a
    // literal `**` before the final segment, preserving its position.
    let unpaired = parts.len().is_multiple_of(2);
    html! {
        @for (i, part) in parts.iter().enumerate() {
            @if i % 2 == 1 && i + 1 < parts.len() {
                strong { (part) }
            } @else if i % 2 == 1 && unpaired {
                "**" (part)
            } @else {
                (part)
            }
        }
    }
}

/// The shared section renderer for the admin detail and preview pages.
/// Snake names are user-controlled, so all text goes through maud's
/// escaping. Mobile: 44px/16px links, `max-width:100%` images, and the
/// container sets `font-size:16px`.
pub fn sections_html(sections: &[super::StoredSection], base_url: &str) -> Markup {
    let base = base_url.trim_end_matches('/');
    html! {
        @for section in sections {
            section style="font-size:16px;margin-bottom:1.5rem" {
                h2 {
                    (section.kind.heading())
                    @if let Some(ladder) = &section.ladder_name {
                        " — " (ladder)
                    }
                }
                p { (text_html(&section.summary)) }
                @if section.items.is_empty() && section.summary != "none this week" {
                    // Empty section whose summary is something else (e.g. a
                    // zero count): still say so — but never repeat the marker
                    // the summary already carries.
                    p { em { "none this week" } }
                } @else {
                    ul style="padding-left:1.25rem" {
                        @for item in &section.items {
                            li style="margin-bottom:1rem" {
                                (text_html(&item.text))
                                @if let Some(game_id) = item.game_id {
                                    div style="margin:0.5rem 0;display:flex;flex-wrap:wrap;gap:0.75rem" {
                                        a href=(format!("{base}/games/{game_id}"))
                                          style="min-height:44px;display:inline-flex;align-items:center;font-size:16px"
                                          { "Replay" }
                                        a href=(format!("{base}/og/games/{game_id}.png?showSpoilers=true"))
                                          style="min-height:44px;display:inline-flex;align-items:center;font-size:16px"
                                          { "Card" }
                                        a href=(crate::routes::game::view::export_gif_url(base, game_id))
                                          style="min-height:44px;display:inline-flex;align-items:center;font-size:16px"
                                          { "Export GIF" }
                                    }
                                    img loading="lazy"
                                        src=(format!("/og/games/{game_id}.png?showSpoilers=true"))
                                        alt=(format!("Final-frame card for game {game_id}"))
                                        style="max-width:100%;height:auto;border:1px solid #ccc;border-radius:6px";
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn truncate(name: &str) -> &str {
    match name.char_indices().nth(100) {
        Some((idx, _)) => &name[..idx],
        None => name,
    }
}

/// Render the store-time Discord snapshot. Every part is conditional — a
/// picks-less week still announces the week and the admin link.
pub fn discord_message(
    digest: &ComputedDigest,
    picks: &DiscordPicks,
    digest_id: Uuid,
    base_url: &str,
) -> String {
    let base = base_url.trim_end_matches('/');
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "**Arena highlights: {} – {}** ({} rated ladder games)",
        digest.window_start.format("%b %-d"),
        digest.window_end.format("%b %-d, %Y"),
        thousands(digest.games_count)
    ));
    if let Some(pick) = &picks.duel_photo_finish
        && let Some(game_id) = pick.game_id
    {
        lines.push(format!(
            "Photo finish: {} — {base}/games/{game_id}",
            truncate(&pick.text)
        ));
    }
    if let Some(pick) = &picks.top_upset
        && let Some(game_id) = pick.game_id
    {
        lines.push(format!(
            "Upset of the week: {} — {base}/games/{game_id}",
            truncate(&pick.text)
        ));
    }
    for change in &picks.throne_changes {
        let crowning = change
            .crowning
            .as_ref()
            .and_then(|pick| pick.game_id)
            .map(|game_id| format!(" — {base}/games/{game_id}"))
            .unwrap_or_default();
        lines.push(format!(
            "#1 changed hands {} time{} on {}: {}{}",
            change.changes,
            if change.changes == 1 { "" } else { "s" },
            truncate(&change.ladder_name),
            change
                .crowning
                .as_ref()
                .map(|pick| truncate(&pick.text))
                .unwrap_or("new leader"),
            crowning
        ));
    }
    lines.push(format!("Full digest: {base}/admin/highlights/{digest_id}"));
    let message = lines.join("\n");
    // Discord caps message content at 2000 chars.
    if message.chars().count() <= 2000 {
        message
    } else {
        message.chars().take(1997).collect::<String>() + "..."
    }
}

/// Warning shown when more than half the window's games lack final frames
/// (archived/pruned by retention).
pub fn frames_missing_banner(digest: &ComputedDigest) -> Option<String> {
    (digest.games_count > 0 && digest.games_without_frames * 2 > digest.games_count).then(|| {
        format!(
            "Frames for {} of {} games are missing (archived/pruned); frame-based sections are incomplete.",
            digest.games_without_frames, digest.games_count
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlights::types::{
        DiscordPicks, Ladder, Pick, Section, SectionItem, SectionKind, ThroneChange,
    };

    fn empty_digest() -> ComputedDigest {
        ComputedDigest {
            window_start: "2026-10-24T12:00:00Z".parse().unwrap(),
            window_end: "2026-10-31T12:00:00Z".parse().unwrap(),
            games_count: 0,
            games_without_frames: 0,
            per_ladder_counts: vec![("Duels 11x11".into(), 0)],
            duel_k: 1.12,
            four_snake_k: 1.98,
            sections: vec![],
            picks: DiscordPicks::default(),
        }
    }

    fn ladder() -> Ladder {
        Ladder {
            leaderboard_id: Uuid::nil(),
            name: "Duels 11x11".to_string(),
            match_size: 2,
        }
    }

    #[test]
    fn markdown_renders_title_counts_and_sections() {
        let game_id = Uuid::parse_str("15fcf7be-9bb8-46ee-8a2b-e728a64f4195").unwrap();
        let digest = ComputedDigest {
            games_count: 22_566,
            per_ladder_counts: vec![
                ("Duels 11x11".into(), 11_152),
                ("Standard 11x11".into(), 5_343),
            ],
            sections: vec![
                Section {
                    kind: SectionKind::PhotoFinishes,
                    ladder: Some(ladder()),
                    summary: "last-turn head-to-head".into(),
                    items: vec![SectionItem {
                        game_id: Some(game_id),
                        subject: Some("Odiodin".into()),
                        value: Some(778.0),
                        text: "**Odiodin** (53) over rattler (52) on turn 778".into(),
                    }],
                },
                Section {
                    kind: SectionKind::Marathons,
                    ladder: Some(ladder()),
                    summary: "none this week".into(),
                    items: vec![],
                },
            ],
            ..empty_digest()
        };
        let markdown = render_markdown(&digest, "https://arena.battlesnake.com");
        assert!(markdown.contains("# Arena highlights: Sat Oct 24 12:00Z to Sat Oct 31 12:00Z"));
        assert!(markdown.contains("22,566 rated ladder games: Duels 11,152, Standard 5,343."));
        assert!(markdown.contains("## Photo finishes"));
        assert!(markdown.contains(
            "[game](https://arena.battlesnake.com/games/15fcf7be-9bb8-46ee-8a2b-e728a64f4195)"
        ));
        assert!(markdown.contains(
            "[card](https://arena.battlesnake.com/og/games/15fcf7be-9bb8-46ee-8a2b-e728a64f4195.png?showSpoilers=true)"
        ));
        assert!(markdown.contains("none this week"));
    }

    #[test]
    fn discord_message_with_all_picks() {
        let game_id = Uuid::new_v4();
        let digest = ComputedDigest {
            games_count: 22_566,
            ..empty_digest()
        };
        let picks = DiscordPicks {
            throne_changes: vec![ThroneChange {
                ladder_name: "Duels 11x11".to_string(),
                changes: 2,
                crowning: Some(Pick {
                    game_id: Some(game_id),
                    text: "Bwukinator took #1 on Duels 11x11 Mon 16:36Z".to_string(),
                }),
            }],
            duel_photo_finish: Some(Pick {
                game_id: Some(game_id),
                text: "Duels photo finish on Duels 11x11: Odiodin over rattler, turn 778"
                    .to_string(),
            }),
            top_upset: Some(Pick {
                game_id: Some(game_id),
                text: "Duels upset on Duels 11x11: slinky-baseline-ppo won at 0.9%".to_string(),
            }),
        };
        let digest_id = Uuid::new_v4();
        let message = discord_message(&digest, &picks, digest_id, "https://arena.battlesnake.com");
        assert!(message.contains("Oct 24 – Oct 31, 2026"));
        assert!(message.contains("22,566 rated ladder games"));
        assert!(message.contains("Photo finish:"));
        assert!(message.contains("Upset of the week:"));
        assert!(message.contains("#1 changed hands 2 times"));
        assert!(message.contains(&format!("/admin/highlights/{digest_id}")));
        assert!(message.len() < 2000);
    }

    #[test]
    fn discord_message_without_picks_still_announces() {
        let digest = empty_digest();
        let digest_id = Uuid::new_v4();
        let message = discord_message(&digest, &DiscordPicks::default(), digest_id, "https://x");
        assert!(message.contains("Arena highlights"));
        assert!(message.contains(&format!("/admin/highlights/{digest_id}")));
    }

    #[test]
    fn discord_message_truncates_pathological_names() {
        let long_name = "S".repeat(500);
        let digest = ComputedDigest {
            games_count: 1,
            ..empty_digest()
        };
        let picks = DiscordPicks {
            duel_photo_finish: Some(Pick {
                game_id: Some(Uuid::new_v4()),
                text: format!("{long_name} photo finish"),
            }),
            ..DiscordPicks::default()
        };
        let message = discord_message(&digest, &picks, Uuid::new_v4(), "https://x");
        assert!(message.chars().count() <= 2000, "{}", message.len());
    }

    #[test]
    fn frames_missing_banner_threshold() {
        let mut digest = empty_digest();
        digest.games_count = 10;
        digest.games_without_frames = 5;
        // Exactly half does not fire (strictly more than half).
        assert!(frames_missing_banner(&digest).is_none());
        digest.games_without_frames = 6;
        assert!(frames_missing_banner(&digest).is_some());
        // Zero games never fires.
        digest.games_count = 0;
        digest.games_without_frames = 0;
        assert!(frames_missing_banner(&digest).is_none());
    }

    #[test]
    fn sections_html_contains_card_links_and_escaping() {
        use crate::highlights::{StoredItem, StoredSection};
        let game_id = Uuid::new_v4();
        let sections = vec![
            StoredSection {
                kind: SectionKind::Upsets,
                ladder_name: Some("Duels 11x11".to_string()),
                position: 0,
                summary: "1 upsets".to_string(),
                items: vec![StoredItem {
                    position: 0,
                    game_id: Some(game_id),
                    subject: Some("<script>alert(1)</script>".to_string()),
                    value: Some(0.01),
                    text: "**<b>Bold & Brave</b>** won".to_string(),
                }],
            },
            StoredSection {
                kind: SectionKind::Marathons,
                ladder_name: Some("Duels 11x11".to_string()),
                position: 1,
                summary: "none this week".to_string(),
                items: vec![],
            },
        ];
        let markup = sections_html(&sections, "https://arena.battlesnake.com").into_string();
        assert!(markup.contains(&format!("/og/games/{game_id}.png?showSpoilers=true")));
        assert!(markup.contains("exporter.battlesnake.com"));
        // Snake names are user-controlled: rendered text is escaped, and
        // the raw subject never reaches the page.
        assert!(!markup.contains("<script>"));
        assert!(markup.contains("&lt;b&gt;Bold &amp; Brave&lt;/b&gt;"));
        // Markdown bold converts to <strong> — no literal `**` in HTML.
        assert!(markup.contains("<strong>&lt;b&gt;Bold &amp; Brave&lt;/b&gt;</strong>"));
        assert!(!markup.contains("**"));
        assert!(markup.contains("min-height:44px"));
        // An empty section says "none this week" exactly once (its summary
        // already carries the marker; no duplicate em line).
        assert_eq!(markup.matches("none this week").count(), 1);
    }

    #[test]
    fn text_html_preserves_unmatched_marker_position() {
        // Unmatched trailing marker stays in place: `foo**bar` renders with
        // the `**` before `bar`, never reordered to the end of the text.
        let html = text_html("foo**bar").into_string();
        let foo = html.find("foo").expect("foo rendered");
        let marker = html.find("**").expect("literal marker rendered");
        let bar = html.find("bar").expect("bar rendered");
        assert!(
            foo < marker && marker < bar,
            "marker must sit between foo and bar: {html}"
        );
        assert!(
            !html.contains("<strong>"),
            "unpaired marker must not bold anything"
        );

        // Paired markers still bold; an unmatched trailing marker stays
        // literal before its own segment.
        let html = text_html("**a**b**c").into_string();
        assert!(html.contains("<strong>a</strong>b**c"), "{html}");

        // Fully paired text has no literal marker.
        assert!(
            text_html("**bold**")
                .into_string()
                .contains("<strong>bold</strong>")
        );
    }
}
