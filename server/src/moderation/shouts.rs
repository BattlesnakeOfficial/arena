//! Post-game shout screening (DEV-1297).
//!
//! After a game finishes, a [`ScreenShoutsJob`] collects the game's
//! distinct (snake, shout) pairs from persisted frames and asks Jev about
//! them in ONE multi-noul call. Violating strings are recorded in
//! `suppressed_shouts`; every frame-serving path blanks a suppressed
//! `(snake, shout)` at serve time — stored frames are never rewritten.
//!
//! Design invariants:
//! - **Fail open**: any Jev error leaves the game unscreened-and-served
//!   (accepted window, same exposure as live games); the one-row-per-game
//!   `shout_screenings` marker is written on every terminal path, so the
//!   job never retries on Jev failures.
//! - **Named object keys only** in Jev `state.shouts` — array-index
//!   accuracy collapses, named keys scored 384/384 in the eval that
//!   produced this design.
//! - **Over-cap strings are suppressed unjudged** (`model = 'over-cap'`,
//!   `probability NULL`): never show what wasn't judged. The judged set is
//!   allocated round-robin across snakes so a chatty snake can't starve an
//!   opponent's constant catchphrase out of the judged set.
//! - **Never log shout text** at INFO — it is attacker-controlled.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::models::shout_moderation::DistinctShout;
use crate::state::AppState;

/// The noul proposition asked for every distinct shout, referencing the
/// record's key so the judgment binds to that specific text: a bare
/// "this text" proposition measured a UNIFORM noul across all keys
/// (0.96 for both "get rekt" and a slur), while key-referencing
/// instructions separated them cleanly (0.33 vs 0.98). Playful
/// competitive trash talk is explicitly allowed by the `state.context`
/// (see [`shouts_state_and_questions`]).
pub(crate) fn shout_noul_instructions(key: &str) -> String {
    format!(
        "The text of the shout stored under shouts.{key} in this state would violate a \
         family-friendly code of conduct (hate speech, sexual content, harassment, or \
         threats) if displayed publicly, including when the meaning is disguised with \
         leetspeak or homoglyphs."
    )
}

/// Tunables, split out from config so tests can drive them directly.
#[derive(Debug, Clone, Copy)]
pub struct ShoutScreeningParams {
    /// Probability at/above which a shout is suppressed. Jev answers vary
    /// sd 0.02–0.05 near a threshold — treat this as a band.
    pub suppress_threshold: f64,
    /// Max distinct shout strings judged per game; the rest are suppressed
    /// unjudged rather than shown.
    pub max_judged: usize,
}

impl Default for ShoutScreeningParams {
    fn default() -> Self {
        Self {
            suppress_threshold: 0.90,
            max_judged: 100,
        }
    }
}

/// Production entry point: reads `state.config.moderation` and delegates.
pub async fn screen_game_shouts(state: &AppState, game_id: Uuid) -> cja::Result<()> {
    let params = ShoutScreeningParams {
        suppress_threshold: state.config.moderation.shout_suppress_threshold,
        max_judged: state.config.moderation.shout_max_judged.max(1) as usize,
    };
    screen_game_shouts_with(state, game_id, &params).await
}

/// Testable core. Never returns `Err` for Jev reasons (fail open); only DB
/// errors propagate (job retry is then fine — nothing was half-written
/// except idempotent suppression rows).
pub async fn screen_game_shouts_with(
    state: &AppState,
    game_id: Uuid,
    params: &ShoutScreeningParams,
) -> cja::Result<()> {
    let pool = &state.db;

    // 1. Idempotency: already screened (duplicate enqueue, racing worker,
    //    or a Finished-retry) → no-op.
    if crate::models::shout_moderation::get_shout_screening(pool, game_id)
        .await?
        .is_some()
    {
        return Ok(());
    }

    // 2. No key configured → feature inert: no rows, no calls. Games
    //    finished while disabled are never retro-screened (accepted).
    if !state.moderation.is_enabled() {
        return Ok(());
    }

    // 3. Collect distinct shouts; none → terminal "no_shouts", no Jev call.
    let rows = crate::models::shout_moderation::distinct_game_shouts(pool, game_id).await?;
    if rows.is_empty() {
        crate::models::shout_moderation::insert_shout_screening(
            pool,
            game_id,
            "no_shouts",
            0,
            0,
            0,
            0,
            None,
            None,
            None,
        )
        .await?;
        return Ok(());
    }
    let distinct_count = rows.len();

    // 4. Split the SQL-ordered rows (snake_id ASC, frequency DESC, shout
    //    ASC) into consecutive per-snake slices, then interleave
    //    round-robin up to the judged cap.
    let mut groups: Vec<&[DistinctShout]> = Vec::new();
    let mut start = 0;
    for i in 1..=rows.len() {
        if i == rows.len() || rows[i].snake_id != rows[start].snake_id {
            groups.push(&rows[start..i]);
            start = i;
        }
    }
    let selected = round_robin(&groups, params.max_judged);
    let selected_keys: HashSet<(&str, &str)> = selected
        .iter()
        .map(|d| (d.snake_id.as_str(), d.text.as_str()))
        .collect();
    let judged: Vec<DistinctShout> = selected.iter().map(|d| (*d).clone()).collect();
    let over_cap: Vec<DistinctShout> = rows
        .iter()
        .filter(|d| !selected_keys.contains(&(d.snake_id.as_str(), d.text.as_str())))
        .cloned()
        .collect();
    let judged_count = judged.len();
    let over_cap_count = over_cap.len();

    // 5. One Jev call for the whole game.
    let (jev_state, questions) = shouts_state_and_questions(&judged);
    let mut suppressed_count: i32 = 0;
    match state.moderation.judge_raw(&jev_state, &questions).await {
        crate::moderation::RawJudgeOutcome::Disabled => {
            // Raced config change; treat as inert.
            return Ok(());
        }
        crate::moderation::RawJudgeOutcome::Errored => {
            // Fail open, no retry: terminal marker, event, done.
            crate::models::shout_moderation::insert_shout_screening(
                pool,
                game_id,
                "jev_error",
                distinct_count as i32,
                0,
                0,
                over_cap_count as i32,
                None,
                None,
                None,
            )
            .await?;
            tracing::info!(
                event_type = "shout_screening",
                outcome = "jev_error",
                game_id = %game_id,
                distinct_shout_count = distinct_count,
                judged_count = judged_count,
                suppressed_count = 0,
                over_cap_count = over_cap_count,
                "Shout screening"
            );
            return Ok(());
        }
        crate::moderation::RawJudgeOutcome::Answered {
            response,
            latency_ms,
        } => {
            // 6. Apply per-key answers. A malformed answer for one key
            //    must not fail the others (keep + warn, key only).
            let mut owners: Option<HashMap<String, (Uuid, Uuid)>> = None;
            for (i, entry) in judged.iter().enumerate() {
                let key = format!("s{}", i + 1);
                let Some(crate::moderation::jev::Answer::Noul { noul }) =
                    response.answers.get(&key)
                else {
                    tracing::warn!(
                        game_id = %game_id,
                        answer_key = %key,
                        "Shout screening: missing or non-noul answer"
                    );
                    continue;
                };
                if *noul < params.suppress_threshold {
                    continue;
                }

                let inserted = crate::models::shout_moderation::insert_suppressed_shout(
                    pool,
                    game_id,
                    &entry.snake_id,
                    &entry.text,
                    Some(*noul),
                    Some(&response.model),
                )
                .await?;
                suppressed_count += 1;

                // Fresh suppression → a moderation_flags row for the admin
                // queue (repeat-offender surfacing). Resolving the owner
                // needs the game's battlesnakes; build the map once.
                if inserted {
                    if owners.is_none() {
                        owners = Some(owner_map(pool, game_id).await);
                    }
                    if let Some(map) = &owners
                        && let Some((battlesnake_id, user_id)) = map.get(&entry.snake_id).copied()
                    {
                        let flag = crate::models::moderation_flag::NewModerationFlag {
                            field_kind: crate::moderation::FieldKind::SnakeShout.as_str(),
                            text: &entry.text,
                            subject_id: Some(battlesnake_id),
                            user_id,
                            decision: "flagged",
                            hate_or_slur: None,
                            sexual_or_graphic: None,
                            harassment_or_threat: None,
                            impersonates_staff_or_platform: None,
                            disguised_evasion: None,
                            action_choice: None,
                            action_confidence: None,
                            action_block_mass: None,
                            model: Some(&response.model),
                        };
                        if let Err(e) =
                            crate::models::moderation_flag::insert_flag(pool, &flag).await
                        {
                            // Non-fatal: the suppression stands; the
                            // admin queue just misses one surfacing row.
                            tracing::error!(
                                game_id = %game_id,
                                snake_id = %entry.snake_id,
                                error = %format!("{e:#}"),
                                "Failed to record shout moderation flag row"
                            );
                        }
                        // Owner unresolvable (archived/imported frame ids):
                        // skip the flags row entirely (user_id NOT NULL).
                    }
                }
            }

            // 7. Over-cap strings: suppressed unjudged, one bulk insert.
            if !over_cap.is_empty() {
                crate::models::shout_moderation::insert_over_cap_suppressions(
                    pool, game_id, &over_cap,
                )
                .await?;
            }

            // 8. Terminal marker LAST: a crash before this point leaves
            //    only idempotent rows and allows one bounded re-run.
            crate::models::shout_moderation::insert_shout_screening(
                pool,
                game_id,
                "screened",
                distinct_count as i32,
                judged_count as i32,
                suppressed_count,
                over_cap_count as i32,
                Some(&response.model),
                Some(latency_ms),
                Some(response.usage.input_tokens as i64),
            )
            .await?;

            // 9. One Eyes event per screened game. NEVER shout text.
            tracing::info!(
                event_type = "shout_screening",
                outcome = "screened",
                game_id = %game_id,
                distinct_shout_count = distinct_count,
                judged_count = judged_count,
                suppressed_count = suppressed_count,
                over_cap_count = over_cap_count,
                latency_ms = latency_ms,
                input_tokens = response.usage.input_tokens,
                model = %response.model,
                "Shout screening"
            );
        }
    }

    Ok(())
}

/// Frame snake id (the runner's `game_battlesnake_id` string) →
/// (battlesnake_id, user_id) for `moderation_flags` rows. `subject_id`
/// uses the battlesnake id, NOT the per-game id, so repeat-offender
/// grouping works across games.
async fn owner_map(pool: &sqlx::PgPool, game_id: Uuid) -> HashMap<String, (Uuid, Uuid)> {
    match crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id).await {
        Ok(snakes) => snakes
            .into_iter()
            .map(|s| {
                (
                    s.game_battlesnake_id.to_string(),
                    (s.battlesnake_id, s.user_id),
                )
            })
            .collect(),
        Err(e) => {
            tracing::error!(
                game_id = %game_id,
                error = %format!("{e:#}"),
                "Shout screening: failed to resolve battlesnake owners"
            );
            HashMap::new()
        }
    }
}

/// Build `(state, questions)` for the judge. Keys `"s1".."sN"` over the
/// judged slice. `state.shouts` uses named object keys ONLY — never an
/// array. Noul questions carry `instructions` only (the API rejects
/// `criteria` on nouls).
pub(crate) fn shouts_state_and_questions(
    judged: &[DistinctShout],
) -> (serde_json::Value, serde_json::Value) {
    let mut shouts = serde_json::Map::new();
    let mut questions = serde_json::Map::new();
    for (i, entry) in judged.iter().enumerate() {
        let key = format!("s{}", i + 1);
        let snake_name = if entry.snake_name.is_empty() {
            entry.snake_id.clone()
        } else {
            entry.snake_name.clone()
        };
        shouts.insert(
            key.clone(),
            serde_json::json!({ "snake": snake_name, "text": entry.text }),
        );
        questions.insert(
            key.clone(),
            serde_json::json!({
                "type": "noul",
                "instructions": shout_noul_instructions(&key),
            }),
        );
    }
    let state = serde_json::json!({
        "context": "Shouts are one-line taunts a snake's code returns each turn in a \
                    programming game; playful competitive trash talk like 'get rekt' or \
                    'I'm coming for you' is normal and allowed.",
        "shouts": serde_json::Value::Object(shouts),
    });
    (state, serde_json::Value::Object(questions))
}

/// Deterministic round-robin interleave over per-snake groups (each group
/// already frequency-DESC from the SQL): first string of every snake, then
/// every snake's second, …, until `cap`. Returns at most `cap` refs.
pub(crate) fn round_robin<'a>(
    per_snake: &[&'a [DistinctShout]],
    cap: usize,
) -> Vec<&'a DistinctShout> {
    let mut selected = Vec::with_capacity(cap.min(per_snake.iter().map(|g| g.len()).sum()));
    'outer: for depth in 0..per_snake.iter().map(|g| g.len()).max().unwrap_or(0) {
        for group in per_snake {
            if depth < group.len() {
                if selected.len() >= cap {
                    break 'outer;
                }
                selected.push(&group[depth]);
            }
        }
    }
    selected
}

/// Serve-time strip: blank `Snakes[i].Shout` when that snake's `(ID,
/// Shout)` is in the map. No-op on empty map; tolerant of shapeless or
/// legacy frames (missing keys, non-array `Snakes`).
pub(crate) fn strip_suppressed_shouts(
    frame: &mut serde_json::Value,
    suppressed: &HashMap<String, HashSet<String>>,
) {
    let Some(snakes) = frame.get_mut("Snakes").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for snake in snakes {
        // Scope the borrows so they end before the write below.
        let suppressed_this_snake = {
            let id = snake.get("ID").and_then(|v| v.as_str());
            let shout = snake.get("Shout").and_then(|v| v.as_str());
            match (id, shout) {
                (Some(id), Some(shout)) => suppressed
                    .get(id)
                    .is_some_and(|texts| texts.contains(shout)),
                _ => false,
            }
        };
        if suppressed_this_snake {
            snake["Shout"] = serde_json::Value::String(String::new());
        }
    }
}

/// Load the serve-time suppression map for a game. Finished/Failed only —
/// live games short-circuit to empty (suppressions cannot exist until
/// post-completion). On DB error: warn and return an empty map (shouts
/// then serve unsuppressed — never fail a frames request over
/// suppression). Lives here because four serve paths use it.
pub(crate) async fn load_suppressed_set(
    db: &sqlx::PgPool,
    game_id: Uuid,
    status: &crate::models::game::GameStatus,
) -> HashMap<String, HashSet<String>> {
    if !matches!(
        status,
        crate::models::game::GameStatus::Finished | crate::models::game::GameStatus::Failed
    ) {
        return HashMap::new();
    }
    match crate::models::shout_moderation::suppressed_shout_pairs(db, game_id).await {
        Ok(pairs) => {
            let mut map: HashMap<String, HashSet<String>> = HashMap::new();
            for (snake_id, text) in pairs {
                map.entry(snake_id).or_default().insert(text);
            }
            map
        }
        Err(e) => {
            tracing::warn!(
                game_id = %game_id,
                error = %format!("{e:#}"),
                "Failed to load suppressed shouts; serving unsuppressed"
            );
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::game::GameStatus;
    use sqlx::PgPool;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- pure-function unit tests ---

    fn shout(id: &str, name: &str, text: &str, frequency: i64) -> DistinctShout {
        DistinctShout {
            snake_id: id.to_string(),
            snake_name: name.to_string(),
            text: text.to_string(),
            frequency,
        }
    }

    #[test]
    fn shouts_state_and_questions_shapes_match() {
        let judged = vec![
            shout("id-1", "Snek", "get rekt", 3),
            shout("id-2", "Noodle", "hello", 1),
        ];
        let (state, questions) = shouts_state_and_questions(&judged);

        let shouts = state.get("shouts").expect("state.shouts");
        assert!(shouts.is_object(), "named object keys, never an array");
        assert!(state.get("context").is_some_and(|c| c.is_string()));
        for key in ["s1", "s2"] {
            assert!(shouts.get(key).is_some(), "state.shouts missing {key}");
            let q = questions.get(key).expect("questions missing {key}");
            assert_eq!(q["type"], "noul");
            assert!(
                q.get("instructions").is_some_and(|i| i.is_string()),
                "{key} needs instructions"
            );
            assert!(
                q.get("criteria").is_none(),
                "{key}: noul questions must NOT carry criteria"
            );
            // Instructions must bind to this key's record (see
            // shout_noul_instructions).
            assert!(
                q["instructions"]
                    .as_str()
                    .is_some_and(|i| i.contains(&format!("shouts.{key}"))),
                "{key} instructions must reference the record key"
            );
        }
        assert_eq!(shouts["s1"]["snake"], "Snek");
        assert_eq!(shouts["s1"]["text"], "get rekt");
        assert_eq!(shouts["s2"]["snake"], "Noodle");
        // Key sets are identical across state.shouts and questions.
        let state_keys: Vec<&str> = shouts
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        let question_keys: Vec<&str> = questions
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(state_keys, question_keys);
    }

    #[test]
    fn shouts_state_falls_back_to_snake_id_for_empty_name() {
        let judged = vec![shout("id-9", "", "hi", 1)];
        let (state, _) = shouts_state_and_questions(&judged);
        assert_eq!(state["shouts"]["s1"]["snake"], "id-9");
    }

    #[test]
    fn round_robin_never_starves_a_snake() {
        let a = [
            shout("a", "A", "a1", 30),
            shout("a", "A", "a2", 20),
            shout("a", "A", "a3", 10),
        ];
        let b = [shout("b", "B", "b1", 5)];
        let groups: Vec<&[DistinctShout]> = vec![&a, &b];

        // Cap 2: first string of every snake, then A's second — B's single
        // string is in, A's a2/a3 wait.
        let selected = round_robin(&groups, 2);
        let texts: Vec<&str> = selected.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts, vec!["a1", "b1"]);

        // Cap >= total: everything, interleaved.
        let selected = round_robin(&groups, 10);
        let texts: Vec<&str> = selected.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts, vec!["a1", "b1", "a2", "a3"]);

        // Degenerate caps: no panic, bounded output.
        assert!(round_robin(&groups, 0).is_empty());
        let selected = round_robin(&groups, 1);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].text, "a1");
        assert!(round_robin(&[], 5).is_empty());
    }

    #[test]
    fn strip_suppressed_shouts_blanks_only_matching_pairs() {
        let mut suppressed: HashMap<String, HashSet<String>> = HashMap::new();
        let mut texts = HashSet::new();
        texts.insert("bad".to_string());
        suppressed.insert("s1".to_string(), texts);

        let mut frame = serde_json::json!({
            "Turn": 3,
            "Snakes": [
                {"ID": "s1", "Name": "A", "Shout": "bad"},      // blanked
                {"ID": "s1", "Name": "A", "Shout": "get rekt"}, // kept
                {"ID": "s2", "Name": "B", "Shout": "bad"},      // kept (wrong snake)
                {"ID": "s3", "Name": "C"},                       // no Shout key
            ],
            "Food": [],
            "Hazards": [],
        });
        strip_suppressed_shouts(&mut frame, &suppressed);
        assert_eq!(frame["Snakes"][0]["Shout"], "");
        assert_eq!(frame["Snakes"][1]["Shout"], "get rekt");
        assert_eq!(frame["Snakes"][2]["Shout"], "bad");
        assert!(frame["Snakes"][3].get("Shout").is_none());
    }

    #[test]
    fn strip_suppressed_shouts_tolerates_shapeless_frames() {
        let mut suppressed: HashMap<String, HashSet<String>> = HashMap::new();
        let mut texts = HashSet::new();
        texts.insert("x".to_string());
        suppressed.insert("s1".to_string(), texts);

        let mut no_snakes = serde_json::json!({"Turn": 0});
        strip_suppressed_shouts(&mut no_snakes, &suppressed);
        assert_eq!(no_snakes, serde_json::json!({"Turn": 0}));

        let mut wrong_type = serde_json::json!({"Snakes": "nope"});
        strip_suppressed_shouts(&mut wrong_type, &suppressed);
        assert_eq!(wrong_type, serde_json::json!({"Snakes": "nope"}));

        let mut frame = serde_json::json!({"Snakes": [{"ID": "s1", "Shout": "x"}]});
        strip_suppressed_shouts(&mut frame, &HashMap::new());
        assert_eq!(frame["Snakes"][0]["Shout"], "x", "empty map is a no-op");
    }

    // --- screening flow tests (wiremock + sqlx) ---

    fn jev_response(nouls: &[f64]) -> serde_json::Value {
        let mut answers = serde_json::Map::new();
        for (i, noul) in nouls.iter().enumerate() {
            answers.insert(
                format!("s{}", i + 1),
                serde_json::json!({"type": "noul", "noul": noul}),
            );
        }
        serde_json::json!({
            "model": "jev-test",
            "answers": serde_json::Value::Object(answers),
            "usage": {"input_tokens": 321, "output_tokens": 12}
        })
    }

    async fn mount_jev(server: &MockServer, body: serde_json::Value, expected: u64) {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(expected)
            .mount(server)
            .await;
    }

    fn test_state(pool: &PgPool, jev_uri: &str) -> AppState {
        let mut state = AppState::test_from_pool(pool.clone());
        state.moderation = crate::moderation::ModerationJudge::new(
            Some("test-key"),
            jev_uri,
            "jev-test",
            Duration::from_millis(500),
            crate::moderation::Thresholds::default(),
        );
        state
    }

    async fn fixture_game(pool: &PgPool, status: &str) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    fn frame(turn: i32, snake_id: &str, name: &str, shout: &str) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{"ID": snake_id, "Name": name, "Shout": shout}],
            "Food": [],
            "Hazards": [],
        })
    }

    fn two_snake_frame(
        turn: i32,
        a: &str,
        a_shout: &str,
        b: &str,
        b_shout: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [
                {"ID": a, "Name": "A", "Shout": a_shout},
                {"ID": b, "Name": "B", "Shout": b_shout},
            ],
            "Food": [],
            "Hazards": [],
        })
    }

    async fn fixture_turn(
        pool: &PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: serde_json::Value,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn_number)
            .bind(frame_data)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// A user + battlesnake + game_battlesnake row, returning
    /// (user_id, battlesnake_id, game_battlesnake_id). The frame snake ID
    /// must be the game_battlesnake_id string.
    async fn fixture_owned_snake(pool: &PgPool, game_id: Uuid) -> cja::Result<(Uuid, Uuid, Uuid)> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (424242, 'test-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'snake', 'http://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?;
        let game_battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement)
             VALUES ($1, $2, 1) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(battlesnake_id)
        .fetch_one(pool)
        .await?;
        Ok((user_id, battlesnake_id, game_battlesnake_id))
    }

    fn params() -> ShoutScreeningParams {
        ShoutScreeningParams {
            suppress_threshold: 0.90,
            max_judged: 100,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn one_offensive_one_benign_exactly_one_suppression(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let (user_id, battlesnake_id, gb_a) = fixture_owned_snake(&pool, game_id).await?;
        // distinct_game_shouts orders by the ID STRING, so make gb_b sort
        // after gb_a deterministically (the mock's noul answers are keyed
        // s1..sN in that order).
        let gb_b = loop {
            let candidate = Uuid::new_v4();
            if candidate.to_string() > gb_a.to_string() {
                break candidate;
            }
        };

        // Snake A: offensive shout on turns 1-3 (one distinct string), plus
        // "get rekt" on turn 0. Snake B: "hello" every turn.
        let offensive = "hate slur text";
        fixture_turn(
            &pool,
            game_id,
            0,
            two_snake_frame(0, &gb_a.to_string(), "get rekt", &gb_b.to_string(), "hello"),
        )
        .await?;
        for turn in 1..=3 {
            fixture_turn(
                &pool,
                game_id,
                turn,
                two_snake_frame(
                    turn,
                    &gb_a.to_string(),
                    offensive,
                    &gb_b.to_string(),
                    "hello",
                ),
            )
            .await?;
        }

        let jev = MockServer::start().await;
        // Order: snake_id ASC, frequency DESC → s1 = A/offensive (freq 3),
        // s2 = A/"get rekt" (freq 1), s3 = B/"hello".
        mount_jev(&jev, jev_response(&[0.97, 0.05, 0.02]), 1).await;

        let state = test_state(&pool, &jev.uri());
        screen_game_shouts_with(&state, game_id, &params()).await?;
        jev.verify().await;

        // Exactly one suppression.
        let rows = sqlx::query!(
            "SELECT snake_id, text, probability, model FROM suppressed_shouts WHERE game_id = $1",
            game_id
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].snake_id, gb_a.to_string());
        assert_eq!(rows[0].text, offensive);
        assert!((rows[0].probability.unwrap() - 0.97).abs() < 1e-9);
        assert_eq!(rows[0].model.as_deref(), Some("jev-test"));

        // One moderation_flags row: flagged, subject = battlesnake_id,
        // owner user_id.
        let flags = sqlx::query!(
            "SELECT field_kind, text, subject_id, user_id, decision FROM moderation_flags
             WHERE user_id = $1",
            user_id
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].field_kind, "snake_shout");
        assert_eq!(flags[0].text, offensive);
        assert_eq!(flags[0].subject_id, Some(battlesnake_id));
        assert_eq!(flags[0].decision, "flagged");

        // Marker with counts (distinct 3, judged 3, suppressed 1, over-cap 0).
        let marker = crate::models::shout_moderation::get_shout_screening(&pool, game_id)
            .await?
            .expect("marker");
        assert_eq!(marker.outcome, "screened");
        assert_eq!(marker.distinct_shout_count, 3);
        assert_eq!(marker.judged_count, 3);
        assert_eq!(marker.suppressed_count, 1);
        assert_eq!(marker.over_cap_count, 0);
        assert_eq!(marker.input_tokens, Some(321));

        // The frames endpoint omits only the offensive shout.
        use axum::extract::{Path, Query, State};
        use axum::response::IntoResponse as _;
        let response = crate::routes::game::api::get_game_frames(
            State(state),
            Path(game_id),
            Query(crate::routes::game::api::FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await
        .unwrap()
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let frames = json["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 4);
        for f in frames {
            let snakes = f["Snakes"].as_array().unwrap();
            let a = snakes.iter().find(|s| s["ID"] == gb_a.to_string()).unwrap();
            let b = snakes.iter().find(|s| s["ID"] == gb_b.to_string()).unwrap();
            assert_eq!(b["Shout"], "hello", "benign shout survives");
            if f["Turn"] == 0 {
                assert_eq!(a["Shout"], "get rekt", "trash talk survives");
            } else {
                assert_eq!(a["Shout"], "", "offensive shout blanked");
            }
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn details_endpoint_strips_too(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let (_user_id, _battlesnake_id, gb_a) = fixture_owned_snake(&pool, game_id).await?;
        let gb_b = Uuid::new_v4();

        let offensive = "awful harassment text";
        fixture_turn(
            &pool,
            game_id,
            0,
            two_snake_frame(0, &gb_a.to_string(), offensive, &gb_b.to_string(), "hello"),
        )
        .await?;

        // Record the suppression directly (serve-path contract, not the
        // screener's).
        crate::models::shout_moderation::insert_suppressed_shout(
            &pool,
            game_id,
            &gb_a.to_string(),
            offensive,
            Some(0.95),
            Some("jev-test"),
        )
        .await?;

        use axum::extract::{Path, State};
        use axum::response::IntoResponse as _;
        fn nobody() -> crate::models::user::User {
            crate::models::user::User {
                user_id: Uuid::nil(),
                external_github_id: 0,
                github_login: "nobody".to_string(),
                github_avatar_url: None,
                github_name: None,
                github_email: None,
                display_name: None,
                pronouns: String::new(),
                country: String::new(),
                backstory: String::new(),
                is_admin: false,
                site_theme: "system".to_string(),
                theater_theme: "match".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }
        let response = crate::routes::api::games::show_game(
            State(AppState::test_from_pool(pool.clone())),
            crate::routes::auth::ApiUser(nobody()),
            Path(game_id),
        )
        .await
        .map_err(|(status, message)| panic!("show_game failed: {status} {message}"))?
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let frames = json["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 1);
        let snakes = frames[0]["Snakes"].as_array().unwrap();
        let a = snakes.iter().find(|s| s["ID"] == gb_a.to_string()).unwrap();
        let b = snakes.iter().find(|s| s["ID"] == gb_b.to_string()).unwrap();
        assert_eq!(a["Shout"], "", "suppressed shout blanked on details path");
        assert_eq!(b["Shout"], "hello");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn no_shouts_makes_no_jev_call(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []}),
        )
        .await?;

        let jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&jev)
            .await;

        let state = test_state(&pool, &jev.uri());
        screen_game_shouts_with(&state, game_id, &params()).await?;
        jev.verify().await;

        let marker = crate::models::shout_moderation::get_shout_screening(&pool, game_id)
            .await?
            .expect("marker");
        assert_eq!(marker.outcome, "no_shouts");
        assert_eq!(marker.distinct_shout_count, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn jev_error_fails_open_and_completes(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let gb = Uuid::new_v4();
        fixture_turn(
            &pool,
            game_id,
            0,
            frame(0, &gb.to_string(), "Snek", "hello"),
        )
        .await?;

        let jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .expect(1)
            .mount(&jev)
            .await;

        let state = test_state(&pool, &jev.uri());
        screen_game_shouts_with(&state, game_id, &params()).await?;
        jev.verify().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM suppressed_shouts WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0, "no suppression on Jev error");
        let marker = crate::models::shout_moderation::get_shout_screening(&pool, game_id)
            .await?
            .expect("terminal marker written");
        assert_eq!(marker.outcome, "jev_error");
        assert_eq!(marker.distinct_shout_count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn over_cap_round_robin_never_starves_a_snake(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let gb_a = "11111111-1111-1111-1111-111111111111";
        let gb_b = "22222222-2222-2222-2222-222222222222";

        // Snake A: 3 distinct shouts (frequency ranked), snake B: 1.
        fixture_turn(
            &pool,
            game_id,
            0,
            two_snake_frame(0, gb_a, "a1", gb_b, "b1"),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            1,
            two_snake_frame(1, gb_a, "a2", gb_b, "b1"),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            2,
            two_snake_frame(2, gb_a, "a1", gb_b, "b1"),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            3,
            two_snake_frame(3, gb_a, "a3", gb_b, "b1"),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            4,
            two_snake_frame(4, gb_a, "a2", gb_b, "b1"),
        )
        .await?;

        let jev = MockServer::start().await;
        // Judged: a1 (freq 2), b1 (freq 5). Both benign.
        Mock::given(method("POST"))
            .and(body_string_contains("b1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jev_response(&[0.01, 0.02])))
            .expect(1)
            .mount(&jev)
            .await;

        let state = test_state(&pool, &jev.uri());
        let tight = ShoutScreeningParams {
            suppress_threshold: 0.90,
            max_judged: 2,
        };
        screen_game_shouts_with(&state, game_id, &tight).await?;
        jev.verify().await;

        let rows = sqlx::query!(
            "SELECT snake_id, text, probability, model FROM suppressed_shouts
             WHERE game_id = $1 ORDER BY text",
            game_id
        )
        .fetch_all(&pool)
        .await?;
        // A's two lowest-frequency strings suppressed unjudged; B's string
        // judged and kept; a1 judged and kept.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.snake_id == gb_a));
        assert!(rows.iter().any(|r| r.text == "a2"));
        assert!(rows.iter().any(|r| r.text == "a3"));
        for row in &rows {
            assert!(row.probability.is_none());
            assert_eq!(row.model.as_deref(), Some("over-cap"));
        }

        // No flags rows: unjudged never surfaces as flagged.
        let flags: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_flags")
            .fetch_one(&pool)
            .await?;
        assert_eq!(flags, 0);

        let marker = crate::models::shout_moderation::get_shout_screening(&pool, game_id)
            .await?
            .expect("marker");
        assert_eq!(marker.outcome, "screened");
        assert_eq!(marker.distinct_shout_count, 4);
        assert_eq!(marker.judged_count, 2);
        assert_eq!(marker.suppressed_count, 0);
        assert_eq!(marker.over_cap_count, 2);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rerun_after_partial_crash_does_not_duplicate_flags(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let (user_id, _battlesnake_id, gb_a) = fixture_owned_snake(&pool, game_id).await?;
        let offensive = "hate slur text";
        fixture_turn(
            &pool,
            game_id,
            0,
            frame(0, &gb_a.to_string(), "A", offensive),
        )
        .await?;

        // Simulate a crash after the suppression + flags write but before
        // the marker: pre-insert both, no marker.
        crate::models::shout_moderation::insert_suppressed_shout(
            &pool,
            game_id,
            &gb_a.to_string(),
            offensive,
            Some(0.99),
            Some("jev-test"),
        )
        .await?;
        crate::models::moderation_flag::insert_flag(
            &pool,
            &crate::models::moderation_flag::NewModerationFlag {
                field_kind: "snake_shout",
                text: offensive,
                subject_id: None,
                user_id,
                decision: "flagged",
                hate_or_slur: None,
                sexual_or_graphic: None,
                harassment_or_threat: None,
                impersonates_staff_or_platform: None,
                disguised_evasion: None,
                action_choice: None,
                action_confidence: None,
                action_block_mass: None,
                model: Some("jev-test"),
            },
        )
        .await?;

        let jev = MockServer::start().await;
        mount_jev(&jev, jev_response(&[0.97]), 1).await;
        let state = test_state(&pool, &jev.uri());
        screen_game_shouts_with(&state, game_id, &params()).await?;
        jev.verify().await;

        // One bounded re-run called Jev once more; rows did not duplicate.
        let suppressions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM suppressed_shouts WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(suppressions, 1);
        let flags: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM moderation_flags WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(flags, 1, "insert returned false → flags skipped");
        assert!(
            crate::models::shout_moderation::get_shout_screening(&pool, game_id)
                .await?
                .is_some(),
            "marker now present"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn second_run_is_a_noop(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let gb = Uuid::new_v4();
        fixture_turn(
            &pool,
            game_id,
            0,
            frame(0, &gb.to_string(), "Snek", "hello"),
        )
        .await?;

        let jev = MockServer::start().await;
        mount_jev(&jev, jev_response(&[0.02]), 1).await;
        let state = test_state(&pool, &jev.uri());
        screen_game_shouts_with(&state, game_id, &params()).await?;

        // Second run: no second Jev call, no duplicate rows.
        screen_game_shouts_with(&state, game_id, &params()).await?;
        assert_eq!(jev.received_requests().await.unwrap().len(), 1);

        let markers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM shout_screenings WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(markers, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_judge_is_inert(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let gb = Uuid::new_v4();
        fixture_turn(
            &pool,
            game_id,
            0,
            frame(0, &gb.to_string(), "Snek", "hello"),
        )
        .await?;

        let state = AppState::test_from_pool(pool.clone());
        screen_game_shouts_with(&state, game_id, &params()).await?;

        let screenings: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM shout_screenings WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(screenings, 0);
        let suppressions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM suppressed_shouts WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(suppressions, 0);
        let flags: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_flags")
            .fetch_one(&pool)
            .await?;
        assert_eq!(flags, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn load_suppressed_set_live_games_short_circuit(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "running").await?;
        crate::models::shout_moderation::insert_suppressed_shout(
            &pool,
            game_id,
            "s1",
            "bad",
            Some(0.99),
            Some("jev-test"),
        )
        .await?;

        // Live: empty regardless of rows.
        assert!(
            load_suppressed_set(&pool, game_id, &GameStatus::Running)
                .await
                .is_empty()
        );
        assert!(
            load_suppressed_set(&pool, game_id, &GameStatus::Waiting)
                .await
                .is_empty()
        );

        // Finished: the pair loads. Failed is also accepted (backfill-ready).
        let map = load_suppressed_set(&pool, game_id, &GameStatus::Finished).await;
        assert!(map["s1"].contains("bad"));
        let map = load_suppressed_set(&pool, game_id, &GameStatus::Failed).await;
        assert!(map["s1"].contains("bad"));
        Ok(())
    }
}

/// Manual threshold eval: needs TYPESAFE_API_KEY + network, writes no DB.
/// Run with:
///   TYPESAFE_API_KEY=... cargo test -p arena --bin arena shout_eval_manual -- --ignored --nocapture
#[cfg(test)]
mod eval {
    use super::*;
    use crate::moderation::{ModerationJudge, RawJudgeOutcome, Thresholds};
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "manual eval: needs TYPESAFE_API_KEY + network; writes no DB"]
    async fn shout_eval_manual() {
        let key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY must be set");
        let endpoint = std::env::var("MODERATION_JEV_URL")
            .unwrap_or_else(|_| crate::moderation::jev::ENDPOINT.to_string());
        let threshold = ShoutScreeningParams::default().suppress_threshold;
        let judge = ModerationJudge::new(
            Some(&key),
            &endpoint,
            "jev-latest",
            Duration::from_millis(8_000),
            Thresholds::default(),
        );

        let cases_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../scripts/shout-eval-cases.txt"
        );
        let content = std::fs::read_to_string(cases_path).expect("eval cases file");

        struct EvalEntry {
            text: String,
            noul: f64,
        }

        let mut category: Option<String> = None;
        let mut per_category: std::collections::BTreeMap<String, Vec<EvalEntry>> =
            std::collections::BTreeMap::new();
        let mut unfiled: Vec<String> = Vec::new();

        // Batch every case into ≤max_judged shouts per Jev call.
        let mut current: Vec<String> = Vec::new();
        let mut current_cats: Vec<String> = Vec::new();
        let max_batch = ShoutScreeningParams::default().max_judged;

        async fn flush(
            judge: &ModerationJudge,
            current: &mut Vec<String>,
            cats: &mut Vec<String>,
            per_category: &mut std::collections::BTreeMap<String, Vec<EvalEntry>>,
        ) {
            if current.is_empty() {
                return;
            }
            let entries: Vec<DistinctShout> = current
                .iter()
                .map(|t| DistinctShout {
                    snake_id: "eval".to_string(),
                    snake_name: "EvalSnake".to_string(),
                    text: t.clone(),
                    frequency: 1,
                })
                .collect();
            let (state, questions) = shouts_state_and_questions(&entries);
            let outcome = judge.judge_raw(&state, &questions).await;
            let RawJudgeOutcome::Answered { response, .. } = outcome else {
                panic!("Jev call failed: {outcome:?}");
            };
            for (i, text) in current.iter().enumerate() {
                let key = format!("s{}", i + 1);
                let noul = match response.answers.get(&key) {
                    Some(crate::moderation::jev::Answer::Noul { noul }) => *noul,
                    _ => f64::NAN,
                };
                per_category
                    .entry(cats[i].clone())
                    .or_default()
                    .push(EvalEntry {
                        text: text.clone(),
                        noul,
                    });
            }
            current.clear();
            cats.clear();
        }

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('#') {
                if let Some(name) = trimmed.strip_prefix("# ") {
                    category = Some(name.trim().to_string());
                }
                continue;
            }
            let Some(cat) = &category else {
                unfiled.push(trimmed.to_string());
                continue;
            };
            current.push(trimmed.to_string());
            current_cats.push(cat.clone());
            if current.len() >= max_batch {
                flush(&judge, &mut current, &mut current_cats, &mut per_category).await;
            }
        }
        flush(&judge, &mut current, &mut current_cats, &mut per_category).await;
        assert!(unfiled.is_empty(), "cases before any # category header");

        println!("\n=== Shout eval (threshold {threshold}) ===");
        for (cat, entries) in &per_category {
            let suppressed = entries.iter().filter(|e| e.noul >= threshold).count();
            let nouls: Vec<f64> = entries
                .iter()
                .map(|e| e.noul)
                .filter(|n| n.is_finite())
                .collect();
            let (min, max, mean, sd) = stats(&nouls);
            println!(
                "{cat:>34}: {} cases | suppressed {suppressed} | noul min/max/mean/sd {min:.3}/{max:.3}/{mean:.3}/{sd:.3}",
                entries.len(),
            );
            for entry in entries.iter().filter(|e| e.noul >= threshold) {
                println!("    SUPPRESSED: {:?} (noul {:.3})", entry.text, entry.noul);
            }
            for entry in entries.iter().filter(|e| !e.noul.is_finite()) {
                println!("    NO ANSWER: {:?}", entry.text);
            }
        }

        // Hard requirement: zero suppressions in playful trash talk and
        // benign categories.
        for cat in ["playful trash talk — MUST NOT suppress", "benign"] {
            if let Some(entries) = per_category.get(cat) {
                for entry in entries {
                    assert!(
                        entry.noul < threshold,
                        "{cat} case {:?} suppressed at {:.3} — raise the threshold",
                        entry.text,
                        entry.noul
                    );
                }
            }
        }

        fn stats(values: &[f64]) -> (f64, f64, f64, f64) {
            if values.is_empty() {
                return (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
            }
            let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
            (min, max, mean, var.sqrt())
        }
    }
}
