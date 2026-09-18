//! Content moderation for user-supplied free text (snake names, tournament
//! name/description, saved-game titles) via the Jev judgment API.
//!
//! One [`moderate_field`] call per submission: a tiny exact-match hard-block
//! pre-check, then (when `TYPESAFE_API_KEY` is configured) a single multi-
//! question Jev call deciding `allow` / `flag_for_review` / `block`.
//! Moderation **never fails a submission**: no key, a judge error, a
//! timeout, or even an audit-row insert failure all fail open. Flagged and
//! blocked submissions are recorded in `moderation_flags` for review.

pub mod jev;

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Decision-independent cap: text longer than this is never moderated at
/// all (no Jev call, no row). Defense in depth — every call site pre-caps
/// (names 64/128 chars, descriptions 4000, titles 100).
pub const MAX_MODERATED_CHARS: usize = 4000;

/// In-flight Jev calls allowed at once. `try_acquire`: beyond this the
/// decision is `unchecked` rather than queueing (registration must not
/// wait on the judge).
const MAX_IN_FLIGHT: usize = 8;

/// Slack added to the client-level timeout beyond the application deadline,
/// so the `tokio::time::timeout` wrapper is what usually fires.
const DEADLINE_RESERVE: Duration = Duration::from_millis(250);

/// The user-visible free-text fields that get moderated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    SnakeName,
    TournamentName,
    TournamentDescription,
    SavedGameTitle,
}

impl FieldKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FieldKind::SnakeName => "snake_name",
            FieldKind::TournamentName => "tournament_name",
            FieldKind::TournamentDescription => "tournament_description",
            FieldKind::SavedGameTitle => "saved_game_title",
        }
    }

    /// Generic, non-accusatory rejection copy. Never reveals which rule
    /// fired.
    pub fn rejection_message(&self) -> &'static str {
        match self {
            FieldKind::SnakeName | FieldKind::TournamentName => {
                "That name isn't allowed. Please choose another."
            }
            FieldKind::TournamentDescription => {
                "That description isn't allowed. Please choose another."
            }
            FieldKind::SavedGameTitle => "That title isn't allowed. Please choose another.",
        }
    }
}

/// The outcome of one moderation judgment.
///
/// `Unchecked` means the judge was configured but could not answer (error,
/// timeout, busy, malformed response): the submission is accepted and
/// recorded so it can be swept later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Flag,
    Block,
    Unchecked,
}

impl Decision {
    /// The decision string used everywhere: the `moderation_decision`
    /// telemetry event and the `moderation_flags.decision` column share
    /// this ONE vocabulary (past tense — what happened to the submission),
    /// so Eyes events and DB rows join with no mapping between them.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Flag => "flagged",
            Decision::Block => "blocked",
            Decision::Unchecked => "unchecked",
        }
    }
}

/// Decision thresholds. Jev answers vary sd 0.02–0.05 between identical
/// calls near a threshold, so treat these as bands, not lines; defaults are
/// provisional until the eval (`docs/moderation-eval-results.md`) and were
/// chosen from its data.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// `probabilities["block"]` mass needed to block. Chosen to sit well
    /// above the observed edgy-but-fine cluster.
    pub block_threshold: f64,
    /// Any single noul at/above this upgrades an otherwise-allow to a flag.
    pub noul_flag_threshold: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            block_threshold: 0.90,
            noul_flag_threshold: 0.60,
        }
    }
}

/// The five noul probabilities from one Jev response.
#[derive(Debug, Clone, Copy, Default)]
pub struct Probabilities {
    pub hate_or_slur: f64,
    pub sexual_or_graphic: f64,
    pub harassment_or_threat: f64,
    pub impersonates_staff_or_platform: f64,
    pub disguised_evasion: f64,
}

/// Everything learned about one moderation check.
#[derive(Debug, Clone)]
pub struct ModerationOutcome {
    pub decision: Decision,
    /// Reported model on success; requested model on failure; `"hardblock"`
    /// for the local list; `None` when the judge is disabled.
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub latency_ms: f64,
    pub probabilities: Option<Probabilities>,
    pub action_choice: Option<String>,
    pub action_confidence: Option<f64>,
    pub action_block_mass: Option<f64>,
}

fn outcome(decision: Decision, model: Option<String>, latency_ms: f64) -> ModerationOutcome {
    ModerationOutcome {
        decision,
        model,
        input_tokens: None,
        latency_ms,
        probabilities: None,
        action_choice: None,
        action_confidence: None,
        action_block_mass: None,
    }
}

struct JudgeInner {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    deadline: Duration,
    in_flight: Arc<tokio::sync::Semaphore>,
    thresholds: Thresholds,
}

fn build_inner(
    api_key: &str,
    endpoint: &str,
    model: &str,
    deadline: Duration,
    thresholds: Thresholds,
) -> cja::Result<JudgeInner> {
    use color_eyre::eyre::Context as _;

    let mut headers = reqwest::header::HeaderMap::new();
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
        .wrap_err("Invalid Jev bearer header value")?;
    value.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, value);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(deadline)
        .timeout(deadline + DEADLINE_RESERVE)
        .build()
        .wrap_err("Failed to build Jev HTTP client")?;

    Ok(JudgeInner {
        client,
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        deadline,
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
        thresholds,
    })
}

/// The judge. [`Clone`]; the inner value is `None` when no API key is
/// configured (no Jev calls at all).
#[derive(Clone, Default)]
pub struct ModerationJudge {
    inner: Option<Arc<JudgeInner>>,
}

impl ModerationJudge {
    pub fn from_config(config: &crate::config::ModerationConfig) -> Self {
        let Some(api_key) = config.api_key.as_deref() else {
            return Self::disabled();
        };
        match build_inner(
            api_key,
            &config.endpoint,
            &config.model,
            Duration::from_millis(config.deadline_ms),
            Thresholds {
                block_threshold: config.block_threshold,
                noul_flag_threshold: config.noul_flag_threshold,
            },
        ) {
            Ok(inner) => Self {
                inner: Some(Arc::new(inner)),
            },
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "Jev moderation client failed to build; moderation disabled");
                Self::disabled()
            }
        }
    }

    /// Wiremock-test constructor. `api_key = None` builds a disabled judge.
    #[cfg(test)]
    pub fn new(
        api_key: Option<&str>,
        endpoint: &str,
        model: &str,
        deadline: Duration,
        thresholds: Thresholds,
    ) -> Self {
        match api_key {
            Some(key) => match build_inner(key, endpoint, model, deadline, thresholds) {
                Ok(inner) => Self {
                    inner: Some(Arc::new(inner)),
                },
                Err(e) => panic!("failed to build test judge: {e:#}"),
            },
            None => Self::disabled(),
        }
    }

    /// A judge that never calls Jev and never writes rows (except via the
    /// offline hard-block list inside [`ModerationJudge::check`]).
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Hard-block pre-check then one Jev call. NEVER errors and never makes
    /// a network call when disabled. Returns `Allow` when disabled (unless
    /// the hard-block list fires).
    pub async fn check(&self, kind: FieldKind, text: &str, is_public: bool) -> ModerationOutcome {
        if is_hard_blocked(text) {
            return outcome(Decision::Block, Some("hardblock".to_string()), 0.0);
        }

        let Some(inner) = &self.inner else {
            return outcome(Decision::Allow, None, 0.0);
        };

        let started = std::time::Instant::now();
        let unchecked = || {
            outcome(
                Decision::Unchecked,
                Some(inner.model.clone()),
                started.elapsed().as_secs_f64() * 1000.0,
            )
        };

        // Busy judge → accept + record, never queue.
        let Ok(_permit) = inner.in_flight.clone().try_acquire_owned() else {
            return unchecked();
        };

        let state = serde_json::json!({
            "field_kind": kind.as_str(),
            "text": text,
            "is_public": is_public,
        });
        let questions = questions();
        let call = jev::system_one(
            &inner.client,
            &inner.endpoint,
            &inner.model,
            &state,
            &questions,
        );
        // The future is lazy: it must be awaited (via the timeout) BEFORE
        // measuring, or `latency_ms` only covers request construction, not
        // the Jev round trip — which would gut the latency telemetry and
        // the `moderation.latency.p95` metric on every success path.
        let result = tokio::time::timeout(inner.deadline, call).await;
        let latency = started.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(Ok(response)) => interpret_response(&response, &inner.thresholds, latency),
            Ok(Err(_)) | Err(_) => unchecked(),
        }
    }
}

/// Turn a parsed Jev response into an outcome, validating that all six
/// answers came back with the right kinds and labels. Any shape problem is
/// `unchecked` (fail open), never a silent allow.
fn interpret_response(
    response: &jev::SystemOneResponse,
    thresholds: &Thresholds,
    latency_ms: f64,
) -> ModerationOutcome {
    let noul = |label: &str| -> Option<f64> {
        match response.answers.get(label) {
            Some(jev::Answer::Noul { noul }) => Some(*noul),
            _ => None,
        }
    };
    let (
        Some(hate_or_slur),
        Some(sexual_or_graphic),
        Some(harassment_or_threat),
        Some(impersonates_staff_or_platform),
        Some(disguised_evasion),
    ) = (
        noul("hate_or_slur"),
        noul("sexual_or_graphic"),
        noul("harassment_or_threat"),
        noul("impersonates_staff_or_platform"),
        noul("disguised_evasion"),
    )
    else {
        return ModerationOutcome {
            input_tokens: Some(response.usage.input_tokens),
            ..outcome(
                Decision::Unchecked,
                Some(response.model.clone()),
                latency_ms,
            )
        };
    };

    let Some(jev::Answer::Choice {
        choice,
        probabilities,
        confidence,
    }) = response.answers.get("action")
    else {
        return ModerationOutcome {
            input_tokens: Some(response.usage.input_tokens),
            ..outcome(
                Decision::Unchecked,
                Some(response.model.clone()),
                latency_ms,
            )
        };
    };
    // Returned-label validation: the chosen action must be one we asked
    // for, and the direct decision signal must be present.
    if !matches!(choice.as_str(), "allow" | "flag_for_review" | "block") {
        return ModerationOutcome {
            input_tokens: Some(response.usage.input_tokens),
            ..outcome(
                Decision::Unchecked,
                Some(response.model.clone()),
                latency_ms,
            )
        };
    }
    let Some(&action_block_mass) = probabilities.get("block") else {
        return ModerationOutcome {
            input_tokens: Some(response.usage.input_tokens),
            ..outcome(
                Decision::Unchecked,
                Some(response.model.clone()),
                latency_ms,
            )
        };
    };

    let probs = Probabilities {
        hate_or_slur,
        sexual_or_graphic,
        harassment_or_threat,
        impersonates_staff_or_platform,
        disguised_evasion,
    };
    let decision = decide(choice, action_block_mass, &probs, thresholds);
    ModerationOutcome {
        probabilities: Some(probs),
        action_choice: Some(choice.clone()),
        action_confidence: Some(*confidence),
        action_block_mass: Some(action_block_mass),
        input_tokens: Some(response.usage.input_tokens),
        ..outcome(decision, Some(response.model.clone()), latency_ms)
    }
}

/// The pure decision policy.
///
/// `action_block_mass` (the choice primitive's `probabilities["block"]`)
/// is the direct signal and wins when the argmax label and the mass
/// disagree: an `allow`-labeled answer with block mass at/above the
/// threshold still blocks, and a `block`-labeled answer below the
/// threshold is a flag, not a silent allow. There is deliberately no zone
/// where Jev leans block yet the submission is admitted with no audit row.
/// Any single noul at/above `noul_flag_threshold` upgrades an allow to a
/// flag.
fn decide(
    action_choice: &str,
    action_block_mass: f64,
    probs: &Probabilities,
    t: &Thresholds,
) -> Decision {
    if action_block_mass >= t.block_threshold {
        Decision::Block
    } else if action_choice == "block"
        || action_choice == "flag_for_review"
        // Jev leaned block (inside the uncertainty band) or explicitly
        // asked for review — never silently admitted with no audit trail.
        // Any single noul at/above the flag threshold upgrades too.
        || [
            probs.hate_or_slur,
            probs.sexual_or_graphic,
            probs.harassment_or_threat,
            probs.impersonates_staff_or_platform,
            probs.disguised_evasion,
        ]
        .iter()
        .any(|p| *p >= t.noul_flag_threshold)
    {
        Decision::Flag
    } else {
        Decision::Allow
    }
}

/// The six questions asked for every moderated field. Noul questions carry
/// `instructions` ONLY (the API rejects `criteria` on nouls); the `criteria`
/// object belongs exclusively to the choice question.
fn questions() -> serde_json::Value {
    serde_json::json!({
        "hate_or_slur": {
            "type": "noul",
            "instructions": "The text contains hate speech or a slur targeting a protected or marginalized group."
        },
        "sexual_or_graphic": {
            "type": "noul",
            "instructions": "The text is sexual, pornographic, or graphically violent."
        },
        "harassment_or_threat": {
            "type": "noul",
            "instructions": "The text harasses, bullies, threatens, or demeans a specific person or group."
        },
        "impersonates_staff_or_platform": {
            "type": "noul",
            "instructions": "The text claims to be Battlesnake or Arena staff, an official account or administrator, or the platform itself (for example a name that reads as an official bot or team account)."
        },
        "disguised_evasion": {
            "type": "noul",
            "instructions": "The text uses leetspeak, homoglyphs, unusual spacing, or zero-width characters to disguise a term that would otherwise violate the community rules."
        },
        "action": {
            "type": "choice",
            "instructions": include_str!("instructions.txt"),
            "criteria": {
                "allow": "Fine for a public programming-game site; competitive or violent-flavored wordplay is okay.",
                "flag_for_review": "Unsure or borderline; a human should look at it.",
                "block": "Clearly against the rules: hate speech, slur, harassment or threat, sexual or graphic content, staff or platform impersonation, or a disguised variant of those."
            }
        }
    })
}

// --- Hard-block pre-check (offline, exact-match) ---

/// Lowercase and keep only alphanumeric characters (removes spaces,
/// punctuation, separators, zero-width characters).
fn normalize_for_hard_block(text: &str) -> String {
    text.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// SHA-256 hex digests of `normalize_for_hard_block(term)` for the worst
/// unambiguous slurs. Exact match on the normalized whole string only —
/// never substring (Scunthorpe protection). Regenerate with:
///   printf '%s' 'TERM' | tr -cd '[:alnum:]' | tr '[:upper:]' '[:lower:]' | sha256sum
const HARD_BLOCK_DIGESTS: &[&str] = &[
    // the hard n-word
    "120f6e5b4ea32f65bda68452fcfaaef06b0136e1d0e4a6f60bc3771fa0936dd6",
    // the f-slur
    "8f5083e3e5c7dc8932f2bf58212f963f3a44752618c96297f82623f736c52738",
    // the k-word
    "c3de533e9b7fe63b79f648687a30d2861edd92fe7c3cd1f2c485e0a605367624",
    // the t-slur
    "16ea09fc78ca83ca502cbcf2377acdf280bf18f61e259153f0868405eedab5ef",
    // the r-slur
    "158869a97379229b7681efae9d7f9c9214134e836d649ba53477c0c111414d59",
];

fn is_hard_blocked(text: &str) -> bool {
    let digest = hex::encode(Sha256::digest(normalize_for_hard_block(text).as_bytes()));
    HARD_BLOCK_DIGESTS.contains(&digest.as_str())
}

/// Hard-block pre-check + one Jev call + best-effort flag-row insert + one
/// tracing event. Never fails the caller: a DB error inserting the audit
/// row is logged (field_kind + decision + user_id, NEVER the text) and the
/// decision is still returned. Block still blocks. Records a
/// `moderation_flags` row for `Flag` / `Block` / `Unchecked`; never for
/// `Allow`. Skips entirely for text longer than [`MAX_MODERATED_CHARS`].
///
/// Safe to run several concurrently (`tokio::join!`): each call is
/// independent — its own HTTP request, semaphore permit, and pool checkout
/// for the insert. Never holds a DB connection across the Jev call.
pub async fn moderate_field(
    db: &sqlx::PgPool,
    judge: &ModerationJudge,
    user_id: Uuid,
    subject_id: Option<Uuid>,
    kind: FieldKind,
    text: &str,
    is_public: bool,
) -> Decision {
    if text.chars().count() > MAX_MODERATED_CHARS {
        tracing::warn!(
            field_kind = kind.as_str(),
            chars = text.chars().count(),
            "Moderation skipped: text exceeds character cap"
        );
        return Decision::Allow;
    }

    let outcome = judge.check(kind, text, is_public).await;
    let decision = outcome.decision;
    let probabilities = outcome.probabilities;

    // One Eyes/tracing event per decision, including Allow. NEVER include
    // the submitted text — it is attacker-controlled and raw text must not
    // land in INFO logs.
    let p = probabilities.as_ref();
    tracing::info!(
        event_type = "moderation_decision",
        field_kind = kind.as_str(),
        decision = decision.as_str(),
        hate_or_slur = p.map(|x| x.hate_or_slur),
        sexual_or_graphic = p.map(|x| x.sexual_or_graphic),
        harassment_or_threat = p.map(|x| x.harassment_or_threat),
        impersonates_staff_or_platform = p.map(|x| x.impersonates_staff_or_platform),
        disguised_evasion = p.map(|x| x.disguised_evasion),
        action_choice = outcome.action_choice.as_deref(),
        action_confidence = outcome.action_confidence,
        action_block_mass = outcome.action_block_mass,
        model = outcome.model.as_deref(),
        latency_ms = outcome.latency_ms,
        input_tokens = outcome.input_tokens,
        "Moderation decision"
    );

    if decision != Decision::Allow {
        let flag = crate::models::moderation_flag::NewModerationFlag {
            field_kind: kind.as_str(),
            text,
            subject_id,
            user_id,
            decision: decision.as_str(),
            hate_or_slur: probabilities.map(|x| x.hate_or_slur),
            sexual_or_graphic: probabilities.map(|x| x.sexual_or_graphic),
            harassment_or_threat: probabilities.map(|x| x.harassment_or_threat),
            impersonates_staff_or_platform: probabilities.map(|x| x.impersonates_staff_or_platform),
            disguised_evasion: probabilities.map(|x| x.disguised_evasion),
            action_choice: outcome.action_choice.as_deref(),
            action_confidence: outcome.action_confidence,
            action_block_mass: outcome.action_block_mass,
            model: outcome.model.as_deref(),
        };
        if let Err(e) = crate::models::moderation_flag::insert_flag(db, &flag).await {
            tracing::error!(
                field_kind = kind.as_str(),
                decision = decision.as_str(),
                user_id = %user_id,
                error = %format!("{e:#}"),
                "Failed to record moderation flag row"
            );
        }
    }

    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_block_mass_at_threshold_blocks() {
        let t = Thresholds::default();
        let probs = Probabilities::default();
        assert_eq!(
            decide("block", 0.97, &probs, &t),
            Decision::Block,
            "high block mass blocks"
        );
        // Mass wins over the argmax label: allow-labeled but 0.95 mass.
        assert_eq!(
            decide("allow", 0.95, &probs, &t),
            Decision::Block,
            "allow-labeled with block mass over threshold still blocks"
        );
    }

    #[test]
    fn decide_block_label_below_threshold_is_flag_never_allow() {
        let t = Thresholds::default();
        let probs = Probabilities::default();
        assert_eq!(decide("block", 0.80, &probs, &t), Decision::Flag);
        assert_eq!(decide("block", 0.60, &probs, &t), Decision::Flag);
        assert_eq!(decide("flag_for_review", 0.30, &probs, &t), Decision::Flag);
    }

    #[test]
    fn decide_noul_upgrade_and_clean_allow() {
        let t = Thresholds::default();
        let clean = Probabilities {
            hate_or_slur: 0.1,
            sexual_or_graphic: 0.1,
            harassment_or_threat: 0.1,
            impersonates_staff_or_platform: 0.1,
            disguised_evasion: 0.1,
        };
        assert_eq!(decide("allow", 0.10, &clean, &t), Decision::Allow);

        let upgraded = Probabilities {
            hate_or_slur: 0.7,
            ..clean
        };
        assert_eq!(
            decide("allow", 0.10, &upgraded, &t),
            Decision::Flag,
            "noul at/above threshold upgrades allow to flag"
        );

        // Just below the noul threshold stays an allow.
        let near = Probabilities {
            hate_or_slur: 0.59,
            ..clean
        };
        assert_eq!(decide("allow", 0.10, &near, &t), Decision::Allow);
    }

    #[test]
    fn normalize_for_hard_block_strips_noise() {
        assert_eq!(normalize_for_hard_block("S n a k e"), "snake");
        assert_eq!(normalize_for_hard_block("Snake-Killer!"), "snakekiller");
        // Zero-width joiner and non-breaking space are stripped too.
        assert_eq!(normalize_for_hard_block("a\u{200b}\u{00a0}b"), "ab");
        assert_eq!(normalize_for_hard_block(""), "");
    }

    #[test]
    fn is_hard_blocked_exact_match_only() {
        // Construct the term at runtime so no slur literal is greppable in
        // test source.
        let term: String = ['n', 'i', 'g', 'g', 'e', 'r'].iter().collect();
        assert!(is_hard_blocked(&term));
        // Case and separators don't evade the exact-match normalization.
        let spaced: String = ['N', ' ', 'I', 'G', 'G', 'e', 'r'].iter().collect();
        assert!(is_hard_blocked(&spaced));

        // Benign names never match (substring matches are forbidden by
        // design — the Scunthorpe rule).
        assert!(!is_hard_blocked("Snake Killer"));
        let near_miss: String = ['s', 'n', 'i', 'g', 'g', 'e', 'r'].iter().collect();
        assert!(
            !is_hard_blocked(&near_miss),
            "names merely containing a fragment must not match"
        );
        assert!(!is_hard_blocked(""));
    }

    #[test]
    fn questions_have_six_labels_and_correct_shapes() {
        let q = questions();
        for label in [
            "hate_or_slur",
            "sexual_or_graphic",
            "harassment_or_threat",
            "impersonates_staff_or_platform",
            "disguised_evasion",
        ] {
            let question = &q[label];
            assert_eq!(question["type"], "noul", "{label}");
            assert!(
                question.get("instructions").is_some_and(|i| i.is_string()),
                "{label} needs instructions"
            );
            assert!(
                question.get("criteria").is_none(),
                "{label}: noul questions must NOT carry criteria"
            );
        }
        let action = &q["action"];
        assert_eq!(action["type"], "choice");
        assert!(action.get("instructions").is_some_and(|i| i.is_string()));
        let criteria = action.get("criteria").expect("choice needs criteria");
        for label in ["allow", "flag_for_review", "block"] {
            assert!(
                criteria.get(label).is_some_and(|c| c.is_string()),
                "criteria missing {label}"
            );
        }
        assert_eq!(criteria.as_object().map(|c| c.len()), Some(3));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn moderate_field_skips_oversize_text_without_call_or_row(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let judge = ModerationJudge::new(
            Some("test-key"),
            &server.uri(),
            "jev-test",
            Duration::from_millis(500),
            Thresholds::default(),
        );
        let user_id = Uuid::new_v4();
        let long = "x".repeat(MAX_MODERATED_CHARS + 1);
        let decision = moderate_field(
            &pool,
            &judge,
            user_id,
            None,
            FieldKind::SnakeName,
            &long,
            true,
        )
        .await;

        assert_eq!(decision, Decision::Allow);
        let rows = sqlx::query_scalar!(
            "SELECT COUNT(*) as \"count!\" FROM moderation_flags WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(rows, 0);
        server.verify().await;
        Ok(())
    }
}

/// Wiremock-driven judge behavior tests — no DB, no handlers.
#[cfg(test)]
mod judge_tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn allow_all_response() -> serde_json::Value {
        response("allow", 0.10, [0.05, 0.02, 0.03, 0.01, 0.04])
    }

    fn response(action: &str, block_mass: f64, nouls: [f64; 5]) -> serde_json::Value {
        let flag_mass = (1.0 - block_mass) / 2.0;
        serde_json::json!({
            "model": "jev-test",
            "answers": {
                "hate_or_slur": {"type": "noul", "noul": nouls[0]},
                "sexual_or_graphic": {"type": "noul", "noul": nouls[1]},
                "harassment_or_threat": {"type": "noul", "noul": nouls[2]},
                "impersonates_staff_or_platform": {"type": "noul", "noul": nouls[3]},
                "disguised_evasion": {"type": "noul", "noul": nouls[4]},
                "action": {
                    "type": "choice",
                    "choice": action,
                    "confidence": 0.9,
                    "probabilities": {
                        "allow": 1.0 - block_mass - flag_mass,
                        "flag_for_review": flag_mass,
                        "block": block_mass
                    }
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 50}
        })
    }

    async fn mount(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    fn judge(server: &MockServer) -> ModerationJudge {
        ModerationJudge::new(
            Some("test-key"),
            &server.uri(),
            "jev-test",
            Duration::from_millis(2_000),
            Thresholds::default(),
        )
    }

    #[tokio::test]
    async fn flagged_response_maps_to_flag() {
        let server = MockServer::start().await;
        mount(&server, response("flag_for_review", 0.30, [0.05; 5])).await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Borderline", true)
            .await;
        assert_eq!(outcome.decision, Decision::Flag);
        assert_eq!(outcome.model.as_deref(), Some("jev-test"));
        server.verify().await;
    }

    #[tokio::test]
    async fn high_block_mass_maps_to_block() {
        let server = MockServer::start().await;
        mount(
            &server,
            response("block", 0.97, [0.9, 0.05, 0.05, 0.05, 0.05]),
        )
        .await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Bad", true)
            .await;
        assert_eq!(outcome.decision, Decision::Block);
        assert_eq!(outcome.action_block_mass, Some(0.97));
        server.verify().await;
    }

    #[tokio::test]
    async fn low_confidence_block_maps_to_flag() {
        let server = MockServer::start().await;
        mount(&server, response("block", 0.60, [0.05; 5])).await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Borderline", true)
            .await;
        assert_eq!(
            outcome.decision,
            Decision::Flag,
            "block label inside the uncertainty band is a flag, never a silent allow"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn timeout_maps_to_unchecked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(allow_all_response())
                    .set_delay(Duration::from_secs(3)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tight = ModerationJudge::new(
            Some("test-key"),
            &server.uri(),
            "jev-test",
            Duration::from_millis(100),
            Thresholds::default(),
        );
        let outcome = tight.check(FieldKind::SnakeName, "Whatever", true).await;
        assert_eq!(outcome.decision, Decision::Unchecked);
        assert_eq!(outcome.model.as_deref(), Some("jev-test"));
    }

    #[tokio::test]
    async fn http_error_maps_to_unchecked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Whatever", true)
            .await;
        assert_eq!(outcome.decision, Decision::Unchecked);
        server.verify().await;
    }

    #[tokio::test]
    async fn missing_block_mass_maps_to_unchecked() {
        let body = serde_json::json!({
            "model": "jev-test",
            "answers": {
                "hate_or_slur": {"type": "noul", "noul": 0.05},
                "sexual_or_graphic": {"type": "noul", "noul": 0.05},
                "harassment_or_threat": {"type": "noul", "noul": 0.05},
                "impersonates_staff_or_platform": {"type": "noul", "noul": 0.05},
                "disguised_evasion": {"type": "noul", "noul": 0.05},
                "action": {
                    "type": "choice",
                    "choice": "allow",
                    "confidence": 0.9,
                    "probabilities": {"allow": 0.7, "flag_for_review": 0.3}
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let server = MockServer::start().await;
        mount(&server, body).await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Whatever", true)
            .await;
        assert_eq!(outcome.decision, Decision::Unchecked);
        server.verify().await;
    }

    #[tokio::test]
    async fn unknown_action_label_maps_to_unchecked() {
        let body = serde_json::json!({
            "model": "jev-test",
            "answers": {
                "hate_or_slur": {"type": "noul", "noul": 0.05},
                "sexual_or_graphic": {"type": "noul", "noul": 0.05},
                "harassment_or_threat": {"type": "noul", "noul": 0.05},
                "impersonates_staff_or_platform": {"type": "noul", "noul": 0.05},
                "disguised_evasion": {"type": "noul", "noul": 0.05},
                "action": {
                    "type": "choice",
                    "choice": "delete_everything",
                    "confidence": 0.9,
                    "probabilities": {"allow": 0.7, "flag_for_review": 0.2, "block": 0.1}
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let server = MockServer::start().await;
        mount(&server, body).await;
        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Whatever", true)
            .await;
        assert_eq!(outcome.decision, Decision::Unchecked);
        server.verify().await;
    }

    #[tokio::test]
    async fn disabled_judge_makes_no_call_and_allows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let judge = ModerationJudge::new(
            None,
            &server.uri(),
            "jev-test",
            Duration::from_millis(100),
            Thresholds::default(),
        );
        assert!(!judge.is_enabled());
        let outcome = judge.check(FieldKind::SnakeName, "Anything", true).await;
        assert_eq!(outcome.decision, Decision::Allow);
        assert_eq!(outcome.model, None);
        server.verify().await;
    }

    #[tokio::test]
    async fn hard_block_hits_without_any_http_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let disabled = ModerationJudge::new(
            None,
            &server.uri(),
            "jev-test",
            Duration::from_millis(100),
            Thresholds::default(),
        );
        let term: String = ['n', 'i', 'g', 'g', 'e', 'r'].iter().collect();
        let outcome = disabled.check(FieldKind::SnakeName, &term, true).await;
        assert_eq!(outcome.decision, Decision::Block);
        assert_eq!(outcome.model.as_deref(), Some("hardblock"));
        server.verify().await;
    }

    #[tokio::test]
    async fn one_post_per_check_with_all_six_questions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"hate_or_slur\""))
            .and(body_string_contains("\"sexual_or_graphic\""))
            .and(body_string_contains("\"harassment_or_threat\""))
            .and(body_string_contains("\"impersonates_staff_or_platform\""))
            .and(body_string_contains("\"disguised_evasion\""))
            .and(body_string_contains("\"action\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(allow_all_response()))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Slitherbot", true)
            .await;
        assert_eq!(outcome.decision, Decision::Allow);
        server.verify().await;
    }

    #[tokio::test]
    async fn state_carries_field_kind_text_and_visibility() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"field_kind\":\"tournament_name\""))
            .and(body_string_contains("\"text\":\"Check Me\""))
            .and(body_string_contains("\"is_public\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(allow_all_response()))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = judge(&server)
            .check(FieldKind::TournamentName, "Check Me", false)
            .await;
        assert_eq!(outcome.decision, Decision::Allow);
        server.verify().await;
    }

    /// PR review pin: the `latency_ms` telemetry field (and the
    /// `moderation.latency.p95` metric built on it) must measure the Jev
    /// round trip on the success path, not the time to construct the
    /// request future.
    #[tokio::test]
    async fn successful_check_reports_round_trip_latency() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(allow_all_response())
                    .set_delay(Duration::from_millis(300)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let outcome = judge(&server)
            .check(FieldKind::SnakeName, "Slitherbot", true)
            .await;
        assert_eq!(outcome.decision, Decision::Allow);
        assert!(
            outcome.latency_ms >= 300.0,
            "latency_ms should cover the ~300ms Jev round trip, got {}",
            outcome.latency_ms
        );
        server.verify().await;
    }
}

/// Handler-level moderation tests: handlers invoked directly as plain async
/// functions (the repo's established pattern — plaintext session cookies
/// fail cja's private-cookie decryption, so no HTTP stack).
#[cfg(test)]
mod moderation_tests {
    use super::*;
    use crate::models::user::User;
    use crate::models::user::get_user_by_id;
    use axum::extract::RawForm;
    use sqlx::PgPool;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn response(action: &str, block_mass: f64, nouls: [f64; 5]) -> serde_json::Value {
        let flag_mass = (1.0 - block_mass) / 2.0;
        serde_json::json!({
            "model": "jev-test",
            "answers": {
                "hate_or_slur": {"type": "noul", "noul": nouls[0]},
                "sexual_or_graphic": {"type": "noul", "noul": nouls[1]},
                "harassment_or_threat": {"type": "noul", "noul": nouls[2]},
                "impersonates_staff_or_platform": {"type": "noul", "noul": nouls[3]},
                "disguised_evasion": {"type": "noul", "noul": nouls[4]},
                "action": {
                    "type": "choice",
                    "choice": action,
                    "confidence": 0.9,
                    "probabilities": {
                        "allow": 1.0 - block_mass - flag_mass,
                        "flag_for_review": flag_mass,
                        "block": block_mass
                    }
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 50}
        })
    }

    fn block_response() -> serde_json::Value {
        response("block", 0.97, [0.9, 0.05, 0.05, 0.05, 0.05])
    }

    fn flag_response() -> serde_json::Value {
        response("flag_for_review", 0.30, [0.65, 0.05, 0.05, 0.05, 0.05])
    }

    fn allow_response() -> serde_json::Value {
        response("allow", 0.10, [0.05; 5])
    }

    async fn mount(server: &MockServer, body: serde_json::Value, expected: u64) {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(expected)
            .mount(server)
            .await;
    }

    /// Like [`mount`] but only for requests whose body contains `needle` —
    /// needed when one server serves different responses per submission.
    async fn mount_matching(
        server: &MockServer,
        needle: &str,
        body: serde_json::Value,
        expected: u64,
    ) {
        Mock::given(method("POST"))
            .and(body_string_contains(needle))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(expected)
            .mount(server)
            .await;
    }

    async fn create_user_row(pool: &PgPool, github_id: i64, is_admin: bool) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token, is_admin)
             VALUES ($1, $2, 'test-token', $3)
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
            is_admin,
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    async fn user_with_session(
        pool: &PgPool,
        github_id: i64,
    ) -> cja::Result<(User, crate::models::session::Session)> {
        let user_id = create_user_row(pool, github_id, false).await?;
        let user = get_user_by_id(pool, user_id)
            .await?
            .expect("just-created user");
        let session = crate::models::session::create_session(pool).await?;
        crate::models::session::associate_user_with_session(pool, session.session_id, user_id)
            .await?;
        Ok((user, session))
    }

    fn test_state(pool: &PgPool, jev_uri: &str) -> crate::state::AppState {
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.moderation = ModerationJudge::new(
            Some("test-key"),
            jev_uri,
            "jev-test",
            Duration::from_millis(1_500),
            Thresholds::default(),
        );
        state
    }

    async fn flag_count(pool: &PgPool, user_id: Uuid) -> i64 {
        sqlx::query_scalar!(
            "SELECT COUNT(*) as \"count!\" FROM moderation_flags WHERE user_id = $1",
            user_id
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn flag_decisions(pool: &PgPool, user_id: Uuid) -> Vec<String> {
        sqlx::query_scalar!(
            "SELECT decision FROM moderation_flags WHERE user_id = $1",
            user_id
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn flash(pool: &PgPool, session_id: Uuid) -> (Option<String>, Option<String>) {
        let session = crate::models::session::get_active_session_by_id(pool, session_id)
            .await
            .unwrap()
            .expect("session exists");
        (session.flash_message, session.flash_type)
    }

    async fn response_body(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Unwrap a handler `ServerResult` (or any `Result<impl IntoResponse, E>`)
    /// into a concrete `Response`, panicking with the handler error on failure.
    trait UnwrapHtml {
        fn unwrap_html(self) -> axum::response::Response;
    }

    impl<S: axum::response::IntoResponse> UnwrapHtml
        for Result<S, crate::errors::ServerError<axum::http::StatusCode>>
    {
        fn unwrap_html(self) -> axum::response::Response {
            match self {
                Ok(response) => response.into_response(),
                Err(e) => panic!("handler failed: {e}"),
            }
        }
    }

    impl<S: axum::response::IntoResponse> UnwrapHtml for Result<S, (axum::http::StatusCode, String)> {
        fn unwrap_html(self) -> axum::response::Response {
            match self {
                Ok(response) => response.into_response(),
                Err((status, message)) => {
                    panic!("JSON-API handler rejected: {status} {message}")
                }
            }
        }
    }

    /// Unwrap an expected `Err((StatusCode, String))` from a JSON-API handler.
    fn expect_api_rejection<S>(
        result: Result<S, (axum::http::StatusCode, String)>,
    ) -> (axum::http::StatusCode, String) {
        match result {
            Err(rejection) => rejection,
            Ok(_) => panic!("expected the submission to be rejected"),
        }
    }

    // --- HTML snake paths ---

    #[sqlx::test(migrations = "../migrations")]
    async fn offensive_name_blocked_on_html_path(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9401).await?;
        let jev = MockServer::start().await;
        mount(&jev, block_response(), 1).await;

        let response = crate::routes::battlesnake::create_battlesnake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            RawForm(axum::body::Bytes::from(
                "name=Bad&url=https://e.co&visibility=public",
            )),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/battlesnakes/new"
        );
        let (message, flash_type) = flash(&pool, session.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("That name isn't allowed. Please choose another.")
        );
        assert_eq!(flash_type.as_deref(), Some("error"));
        assert!(
            crate::models::battlesnake::get_battlesnakes_by_user_id(&pool, user.user_id)
                .await?
                .is_empty()
        );
        assert_eq!(flag_count(&pool, user.user_id).await, 1);
        assert_eq!(
            flag_decisions(&pool, user.user_id).await,
            vec!["blocked".to_string()]
        );
        jev.verify().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn offensive_name_blocked_on_api_path(pool: PgPool) -> cja::Result<()> {
        let (user, _session) = user_with_session(&pool, 9402).await?;
        let jev = MockServer::start().await;
        mount(&jev, block_response(), 1).await;

        let (status, message) = expect_api_rejection(
            crate::routes::api::snakes::create_snake(
                axum::extract::State(test_state(&pool, &jev.uri())),
                crate::routes::auth::ApiUser(user.clone()),
                axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                    name: "Bad".to_string(),
                    url: "https://e.co".to_string(),
                    is_public: true,
                }),
            )
            .await,
        );
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(message, "That name isn't allowed. Please choose another.");
        assert!(
            crate::models::battlesnake::get_battlesnakes_by_user_id(&pool, user.user_id)
                .await?
                .is_empty()
        );
        assert_eq!(
            flag_decisions(&pool, user.user_id).await,
            vec!["blocked".to_string()]
        );
        jev.verify().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn borderline_name_accepted_and_flagged(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9403).await?;
        let jev = MockServer::start().await;
        mount(&jev, flag_response(), 1).await;

        let response = crate::routes::battlesnake::create_battlesnake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            RawForm(axum::body::Bytes::from(
                "name=Borderline&url=https://e.co&visibility=public",
            )),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        let snakes =
            crate::models::battlesnake::get_battlesnakes_by_user_id(&pool, user.user_id).await?;
        assert_eq!(snakes.len(), 1);
        assert_eq!(snakes[0].name, "Borderline");

        assert_eq!(
            flag_decisions(&pool, user.user_id).await,
            vec!["flagged".to_string()]
        );
        let row = sqlx::query!(
            "SELECT hate_or_slur, sexual_or_graphic, harassment_or_threat,
                    impersonates_staff_or_platform, disguised_evasion,
                    action_choice, action_block_mass
             FROM moderation_flags WHERE user_id = $1",
            user.user_id
        )
        .fetch_one(&pool)
        .await?;
        assert!(row.hate_or_slur.is_some());
        assert!(row.sexual_or_graphic.is_some());
        assert!(row.harassment_or_threat.is_some());
        assert!(row.impersonates_staff_or_platform.is_some());
        assert!(row.disguised_evasion.is_some());
        assert_eq!(row.action_choice.as_deref(), Some("flag_for_review"));
        assert!((row.action_block_mass.unwrap() - 0.30).abs() < 1e-9);
        jev.verify().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn benign_name_no_row(pool: PgPool) -> cja::Result<()> {
        let (user, _session) = user_with_session(&pool, 9404).await?;
        let jev = MockServer::start().await;
        mount(&jev, allow_response(), 1).await;

        let response = crate::routes::api::snakes::create_snake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::ApiUser(user.clone()),
            axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                name: "Slitherbot".to_string(),
                url: "https://e.co".to_string(),
                is_public: true,
            }),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(
            crate::models::battlesnake::get_battlesnakes_by_user_id(&pool, user.user_id)
                .await?
                .len(),
            1
        );
        assert_eq!(flag_count(&pool, user.user_id).await, 0);
        jev.verify().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn jev_timeout_accepts_and_records_unchecked(pool: PgPool) -> cja::Result<()> {
        let (user, _session) = user_with_session(&pool, 9405).await?;
        let jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(allow_response())
                    .set_delay(Duration::from_secs(3)),
            )
            .expect(1)
            .mount(&jev)
            .await;

        let mut state = test_state(&pool, &jev.uri());
        state.moderation = ModerationJudge::new(
            Some("test-key"),
            &jev.uri(),
            "jev-test",
            Duration::from_millis(100),
            Thresholds::default(),
        );

        let response = crate::routes::api::snakes::create_snake(
            axum::extract::State(state),
            crate::routes::auth::ApiUser(user.clone()),
            axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                name: "SlowJudge".to_string(),
                url: "https://e.co".to_string(),
                is_public: true,
            }),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(
            crate::models::battlesnake::get_battlesnakes_by_user_id(&pool, user.user_id)
                .await?
                .len(),
            1
        );
        assert_eq!(
            flag_decisions(&pool, user.user_id).await,
            vec!["unchecked".to_string()]
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unset_key_makes_no_call_and_creates_no_row(pool: PgPool) -> cja::Result<()> {
        let (user, _session) = user_with_session(&pool, 9406).await?;
        let jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&jev)
            .await;

        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.moderation = ModerationJudge::disabled();

        let response = crate::routes::api::snakes::create_snake(
            axum::extract::State(state),
            crate::routes::auth::ApiUser(user.clone()),
            axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                name: "NoKey".to_string(),
                url: "https://e.co".to_string(),
                is_public: true,
            }),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(flag_count(&pool, user.user_id).await, 0);
        // Judge-behavior tests prove disabled => no HTTP call; verify here too.
        jev.verify().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn flagged_name_not_relayed_to_discord_but_allowed_name_is(
        pool: PgPool,
    ) -> cja::Result<()> {
        let (user, _session) = user_with_session(&pool, 9407).await?;
        let jev = MockServer::start().await;
        let discord = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&discord)
            .await;

        // Flagged: created, but no Discord post.
        mount_matching(&jev, "FlaggedName", flag_response(), 1).await;
        let state = discord_state(&pool, &jev.uri(), &discord.uri());
        let response = crate::routes::api::snakes::create_snake(
            axum::extract::State(state),
            crate::routes::auth::ApiUser(user.clone()),
            axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                name: "FlaggedName".to_string(),
                url: "https://e.co".to_string(),
                is_public: true,
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);

        // notify_snake_registered is spawned fire-and-forget: give a wrong
        // delivery time to surface, then assert none arrived.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                discord
                    .received_requests()
                    .await
                    .unwrap_or_default()
                    .is_empty(),
                "flagged name must not reach Discord"
            );
        }

        // Positive control: allowed public name relays exactly once.
        mount_matching(&jev, "RelayMe", allow_response(), 1).await;
        let state = discord_state(&pool, &jev.uri(), &discord.uri());
        let response = crate::routes::api::snakes::create_snake(
            axum::extract::State(state),
            crate::routes::auth::ApiUser(user.clone()),
            axum::Json(crate::routes::api::snakes::CreateSnakeRequest {
                name: "RelayMe".to_string(),
                url: "https://e.co/x".to_string(),
                is_public: true,
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);

        let mut delivered = false;
        for _ in 0..20 {
            if !discord
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
            {
                delivered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(delivered, "allowed public name should reach Discord");
        discord.verify().await;
        let requests = discord.received_requests().await.unwrap_or_default();
        assert_eq!(requests.len(), 1);
        Ok(())
    }

    fn discord_state(pool: &PgPool, jev_uri: &str, discord_uri: &str) -> crate::state::AppState {
        let mut state = test_state(pool, jev_uri);
        state.discord = crate::discord::DiscordNotifier::new(
            Some(discord_uri.to_string()),
            reqwest::Client::new(),
        );
        state
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn update_without_name_change_skips_moderation(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9408).await?;
        let jev = MockServer::start().await;
        mount(&jev, allow_response(), 1).await; // one call: creation only

        let state = test_state(&pool, &jev.uri());
        let create = crate::routes::battlesnake::create_battlesnake(
            axum::extract::State(state),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            RawForm(axum::body::Bytes::from(
                "name=Slitherbot&url=https://e.co&visibility=public",
            )),
        )
        .await
        .unwrap_html();
        let snake_id = snake_id_for(&pool, user.user_id, "Slitherbot").await;
        assert_eq!(create.status(), axum::http::StatusCode::SEE_OTHER);

        // Same name, new URL: no second Jev call.
        let response = crate::routes::battlesnake::update_battlesnake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(snake_id),
            RawForm(axum::body::Bytes::from(
                "name=Slitherbot&url=https://e.co/changed&visibility=public",
            )),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

        // Trailing-space trim case: "Slitherbot " still counts as unchanged.
        let response = crate::routes::battlesnake::update_battlesnake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(snake_id),
            RawForm(axum::body::Bytes::from(
                "name=Slitherbot+&url=https://e.co/changed&visibility=public",
            )),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

        // API update without a name field: still no second call.
        let response = crate::routes::api::snakes::update_snake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::ApiUser(user.clone()),
            axum::extract::Path(snake_id),
            axum::Json(crate::routes::api::snakes::UpdateSnakeRequest {
                name: None,
                url: Some("https://e.co/again".to_string()),
                is_public: None,
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Creation was the one and only Jev call.
        jev.verify().await;
        Ok(())
    }

    async fn snake_id_for(pool: &PgPool, user_id: Uuid, name: &str) -> Uuid {
        crate::models::battlesnake::get_battlesnakes_by_user_id(pool, user_id)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("snake {name} not found"))
            .battlesnake_id
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn update_with_new_name_blocks(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9409).await?;
        let jev = MockServer::start().await;
        mount_matching(&jev, "GoodName", allow_response(), 1).await; // create
        mount_matching(&jev, "BadRename", block_response(), 1).await; // rename attempt

        let state = test_state(&pool, &jev.uri());
        crate::routes::battlesnake::create_battlesnake(
            axum::extract::State(state),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            RawForm(axum::body::Bytes::from(
                "name=GoodName&url=https://e.co&visibility=public",
            )),
        )
        .await
        .unwrap_html();
        let snake_id = snake_id_for(&pool, user.user_id, "GoodName").await;

        let response = crate::routes::battlesnake::update_battlesnake(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(snake_id),
            RawForm(axum::body::Bytes::from(
                "name=BadRename&url=https://e.co&visibility=public",
            )),
        )
        .await
        .unwrap_html();

        let edit_path = format!("/battlesnakes/{snake_id}/edit");
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            edit_path
        );
        let (message, flash_type) = flash(&pool, session.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("That name isn't allowed. Please choose another.")
        );
        assert_eq!(flash_type.as_deref(), Some("error"));
        // Name unchanged in the DB.
        assert!(
            snake_id_for(&pool, user.user_id, "GoodName").await == snake_id,
            "name must be unchanged"
        );
        jev.verify().await;
        Ok(())
    }

    // --- Saved games ---

    #[sqlx::test(migrations = "../migrations")]
    async fn saved_game_title_moderated_and_blocked_then_skips_unchanged(
        pool: PgPool,
    ) -> cja::Result<()> {
        use crate::models::game::{CreateGame, GameBoardSize, GameType};

        let (user, session) = user_with_session(&pool, 9410).await?;
        let game = crate::models::game::create_game(
            &pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        let game_id = game.game_id;

        // Blocked title: redirect back, flash, no row.
        let jev_block = MockServer::start().await;
        mount(&jev_block, block_response(), 1).await;
        let response = crate::routes::saved_games::save_game(
            axum::extract::State(test_state(&pool, &jev_block.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(game_id),
            axum::Form(crate::routes::saved_games::SaveGameForm {
                title: Some("Bad Title".to_string()),
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("/games/{game_id}")
        );
        let (message, flash_type) = flash(&pool, session.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("That title isn't allowed. Please choose another.")
        );
        assert_eq!(flash_type.as_deref(), Some("error"));
        assert!(
            crate::models::saved_game::get_saved_game_for_user_and_game(
                &pool,
                user.user_id,
                game_id
            )
            .await?
            .is_none()
        );
        jev_block.verify().await;

        // Allowed title: row created.
        let jev_allow = MockServer::start().await;
        mount(&jev_allow, allow_response(), 1).await;
        let response = crate::routes::saved_games::save_game(
            axum::extract::State(test_state(&pool, &jev_allow.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(game_id),
            axum::Form(crate::routes::saved_games::SaveGameForm {
                title: Some("Fine Title".to_string()),
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        jev_allow.verify().await;

        // Re-save with the SAME title: no second Jev call on this server.
        let response = crate::routes::saved_games::save_game(
            axum::extract::State(test_state(&pool, &jev_allow.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(game_id),
            axum::Form(crate::routes::saved_games::SaveGameForm {
                title: Some("Fine Title".to_string()),
            }),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        jev_allow.verify().await; // still exactly 1 hit
        Ok(())
    }

    // --- Tournaments ---

    fn tournament_form(
        name: &str,
        description: &str,
    ) -> crate::routes::tournament::TournamentSettingsForm {
        use crate::models::tournament::{MatchStyle, RegistrationStatus, TournamentVisibility};
        crate::routes::tournament::TournamentSettingsForm {
            name: name.to_string(),
            description: description.to_string(),
            game_type: "standard".to_string(),
            board_size: "11x11".to_string(),
            match_style: MatchStyle::SingleGame,
            registration_status: RegistrationStatus::Open,
            visibility: TournamentVisibility::Public,
            max_snakes_per_user: 2,
            required_participants: 2,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn tournament_name_blocked_on_create(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9411).await?;
        let jev = MockServer::start().await;
        mount(&jev, block_response(), 1).await;

        let response = crate::routes::tournament::create_tournament(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::Form(tournament_form("Bad Tournament", "")),
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/tournaments/new"
        );
        let (message, flash_type) = flash(&pool, session.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("That name isn't allowed. Please choose another.")
        );
        assert_eq!(flash_type.as_deref(), Some("error"));
        let tournaments = sqlx::query_scalar!(
            "SELECT COUNT(*) as \"count!\" FROM tournaments WHERE user_id = $1",
            user.user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(tournaments, 0);
        jev.verify().await;
        Ok(())
    }

    async fn seed_tournament(pool: &PgPool, user_id: Uuid) -> cja::Result<Uuid> {
        use crate::models::game::{GameBoardSize, GameType};
        use crate::models::tournament::{
            CreateTournament, MatchStyle, RegistrationStatus, TournamentVisibility,
        };
        let t = crate::models::tournament::create_tournament(
            pool,
            user_id,
            CreateTournament {
                name: "Original Name".to_string(),
                description: Some("Original description".to_string()),
                game_type: GameType::Standard,
                board_size: GameBoardSize::Medium,
                registration_status: RegistrationStatus::Open,
                visibility: TournamentVisibility::Public,
                match_style: MatchStyle::SingleGame,
                max_snakes_per_user: 2,
                required_participants: 2,
            },
        )
        .await?;
        Ok(t.tournament_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn tournament_fields_moderated_concurrently(pool: PgPool) -> cja::Result<()> {
        let (user, session) = user_with_session(&pool, 9412).await?;
        let tournament_id = seed_tournament(&pool, user.user_id).await?;

        let jev = MockServer::start().await;
        // Two expectations: one per field kind, each delayed 500ms. If the
        // handler ran them sequentially the wall time would be >= 1s.
        Mock::given(method("POST"))
            .and(body_string_contains("\"field_kind\":\"tournament_name\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(allow_response())
                    .set_delay(Duration::from_millis(500)),
            )
            .expect(1)
            .mount(&jev)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                "\"field_kind\":\"tournament_description\"",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(allow_response())
                    .set_delay(Duration::from_millis(500)),
            )
            .expect(1)
            .mount(&jev)
            .await;

        let started = std::time::Instant::now();
        let response = crate::routes::tournament::update_settings(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user.clone(),
                session: session.clone(),
            },
            axum::extract::Path(tournament_id),
            axum::Form(tournament_form("Renamed Tournament", "New description")),
        )
        .await
        .unwrap_html();
        let elapsed = started.elapsed().as_millis();

        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert!(
            elapsed < 850,
            "two 500ms delays must overlap (took {elapsed}ms; sequential would be >=1000ms)"
        );
        jev.verify().await;

        let row = sqlx::query!(
            "SELECT name, description FROM tournaments WHERE tournament_id = $1",
            tournament_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.name, "Renamed Tournament");
        assert_eq!(row.description.as_deref(), Some("New description"));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn update_settings_pre_validates_before_moderation(pool: PgPool) -> cja::Result<()> {
        // Case A: oversize description is rejected before any Jev call.
        let (user_a, session_a) = user_with_session(&pool, 9413).await?;
        let tournament_a = seed_tournament(&pool, user_a.user_id).await?;
        let jev = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(allow_response()))
            .expect(0)
            .mount(&jev)
            .await;

        let oversize = "d".repeat(4_001);
        let response = crate::routes::tournament::update_settings(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user_a.clone(),
                session: session_a.clone(),
            },
            axum::extract::Path(tournament_a),
            axum::Form(tournament_form("Fine Name", &oversize)),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        let (message, _) = flash(&pool, session_a.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("Description must be at most 4000 characters")
        );
        assert_eq!(flag_count(&pool, user_a.user_id).await, 0);
        jev.verify().await;

        // Case B: a started tournament refuses edits before any Jev call.
        let (user_b, session_b) = user_with_session(&pool, 9414).await?;
        let tournament_b = seed_tournament(&pool, user_b.user_id).await?;
        sqlx::query!(
            "UPDATE tournaments SET status = 'in_progress' WHERE tournament_id = $1",
            tournament_b
        )
        .execute(&pool)
        .await?;

        let response = crate::routes::tournament::update_settings(
            axum::extract::State(test_state(&pool, &jev.uri())),
            crate::routes::auth::CurrentUserWithSession {
                user: user_b.clone(),
                session: session_b.clone(),
            },
            axum::extract::Path(tournament_b),
            axum::Form(tournament_form("Attempted Rename", "")),
        )
        .await
        .unwrap_html();
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        let (message, _) = flash(&pool, session_b.session_id).await;
        assert_eq!(
            message.as_deref(),
            Some("Tournament settings can only be edited before the tournament starts")
        );
        assert_eq!(flag_count(&pool, user_b.user_id).await, 0);
        jev.verify().await;
        Ok(())
    }

    // --- Admin page ---

    #[sqlx::test(migrations = "../migrations")]
    async fn admin_page_renders_for_admin(pool: PgPool) -> cja::Result<()> {
        let admin_id = create_user_row(&pool, 9415, true).await?;
        let admin = get_user_by_id(&pool, admin_id).await?.expect("admin user");

        let (owner, _session) = user_with_session(&pool, 9416).await?;
        crate::models::moderation_flag::insert_flag(
            &pool,
            &crate::models::moderation_flag::NewModerationFlag {
                field_kind: "snake_name",
                text: "Suspicious Name",
                subject_id: None,
                user_id: owner.user_id,
                decision: "flagged",
                hate_or_slur: Some(0.42),
                sexual_or_graphic: Some(0.02),
                harassment_or_threat: Some(0.03),
                impersonates_staff_or_platform: Some(0.01),
                disguised_evasion: Some(0.04),
                action_choice: Some("flag_for_review"),
                action_confidence: Some(0.5),
                action_block_mass: Some(0.33),
                model: Some("jev-test"),
            },
        )
        .await?;

        let page_factory = crate::components::page_factory::PageFactory {
            flash: crate::components::flash::Flash {
                message: None,
                flash_type: None,
            },
            user: None,
            path: "/admin/moderation".to_string(),
            base_url: "http://localhost".to_string(),
        };

        let response = crate::routes::admin::moderation_queue(
            axum::extract::State(crate::state::AppState::test_from_pool(pool.clone())),
            crate::routes::auth::AdminUser(admin),
            page_factory,
        )
        .await
        .unwrap_html();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response_body(response).await;
        assert!(body.contains("Moderation Queue"), "title missing");
        assert!(body.contains("Suspicious Name"), "flagged text missing");
        assert!(body.contains("gh-user-9416"), "owner login missing");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn admin_route_is_gated_by_admin_extractor(pool: PgPool) -> cja::Result<()> {
        // Non-admin session -> 403; admin session -> Ok. Driven through the
        // real AdminUser extractor with a hand-encrypted private cookie —
        // the exact path the middleware takes.
        let (_user, session) = user_with_session(&pool, 9417).await?;
        let admin_id = create_user_row(&pool, 9418, true).await?;
        let admin_session = crate::models::session::create_session(&pool).await?;
        crate::models::session::associate_user_with_session(
            &pool,
            admin_session.session_id,
            admin_id,
        )
        .await?;

        let state = crate::state::AppState::test_from_pool(pool.clone());

        // Non-admin: extractor rejects with 403.
        let outcome = extract_admin_user(&state, &session.session_id.to_string()).await;
        let rejection = match outcome {
            Err(rejection) => rejection,
            Ok(_) => panic!("non-admin must be rejected"),
        };
        assert_eq!(rejection.status(), axum::http::StatusCode::FORBIDDEN);

        // Admin: extractor succeeds.
        let ok = extract_admin_user(&state, &admin_session.session_id.to_string()).await;
        assert!(ok.is_ok());
        Ok(())
    }

    async fn extract_admin_user(
        state: &crate::state::AppState,
        session_id: &str,
    ) -> Result<crate::routes::auth::AdminUser, Box<axum::response::Response>> {
        use axum::extract::FromRequestParts;

        let cookies = tower_cookies::Cookies::default();
        cookies
            .private(&state.cookie_key.0)
            .add(tower_cookies::Cookie::new(
                crate::models::session::SESSION_COOKIE_NAME,
                session_id.to_string(),
            ));
        let mut request = axum::http::Request::get("/admin/moderation")
            .body(())
            .unwrap();
        request.extensions_mut().insert(cookies);
        let (mut parts, _) = request.into_parts();

        crate::routes::auth::AdminUser::from_request_parts(&mut parts, state)
            .await
            .map_err(Box::new)
    }
}

/// Manual threshold eval: needs TYPESAFE_API_KEY + network, writes no DB.
/// Run with:
///   TYPESAFE_API_KEY=... cargo test -p arena --bin arena moderation_eval_manual -- --ignored --nocapture
#[cfg(test)]
mod eval {
    use super::*;

    #[tokio::test]
    #[ignore = "manual eval: needs TYPESAFE_API_KEY + network; writes no DB"]
    async fn moderation_eval_manual() {
        let key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY must be set");
        let endpoint =
            std::env::var("MODERATION_JEV_URL").unwrap_or_else(|_| jev::ENDPOINT.to_string());
        let thresholds = Thresholds::default();
        let judge = ModerationJudge::new(
            Some(&key),
            &endpoint,
            "jev-latest",
            Duration::from_millis(8_000),
            thresholds,
        );

        let names_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../scripts/moderation-eval-names.txt"
        );
        let content = std::fs::read_to_string(names_path).expect("eval names file");
        struct EvalEntry {
            name: String,
            decision: Decision,
            block_mass: f64,
            carried_noul: bool,
            upgrade_driven: bool,
        }

        let mut category: Option<String> = None;
        let mut per_category: std::collections::BTreeMap<String, Vec<EvalEntry>> =
            std::collections::BTreeMap::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(name) = trimmed.strip_prefix("# ") {
                category = Some(name.trim().to_string());
            }
            if trimmed.starts_with('#') {
                continue;
            }
            let Some(cat) = &category else {
                panic!("name {trimmed:?} before any # category header");
            };
            let outcome = judge.check(FieldKind::SnakeName, trimmed, true).await;
            let block_mass = outcome.action_block_mass.unwrap_or(f64::NAN);
            let carried_noul = outcome.probabilities.is_some_and(|p| {
                [
                    p.hate_or_slur,
                    p.sexual_or_graphic,
                    p.harassment_or_threat,
                    p.impersonates_staff_or_platform,
                    p.disguised_evasion,
                ]
                .iter()
                .any(|v| *v >= thresholds.noul_flag_threshold)
            });
            // A flag with an `allow`-labeled action (and mass under the
            // block threshold — else it would be Block) can ONLY come from
            // the noul upgrade: this is the count where the noul threshold
            // itself flipped the decision, as opposed to the name merely
            // carrying a qualifying noul alongside an action-driven flag.
            let upgrade_driven = outcome.decision == Decision::Flag
                && outcome.action_choice.as_deref() == Some("allow");
            println!(
                "{cat:>28} | {trimmed:<30} | {:>6} | mass {block_mass:.3} | action {} | carried_noul {carried_noul} | upgrade_driven {upgrade_driven}",
                outcome.decision.as_str(),
                outcome.action_choice.as_deref().unwrap_or("-"),
            );
            per_category
                .entry(cat.clone())
                .or_default()
                .push(EvalEntry {
                    name: trimmed.to_string(),
                    decision: outcome.decision,
                    block_mass,
                    carried_noul,
                    upgrade_driven,
                });
        }

        println!("\n=== Summary ===");
        for (cat, entries) in &per_category {
            let count = |d: Decision| entries.iter().filter(|e| e.decision == d).count();
            let masses: Vec<f64> = entries
                .iter()
                .filter(|e| e.block_mass.is_finite())
                .map(|e| e.block_mass)
                .collect();
            let (min, mean, sd) = stats(&masses);
            let upgrade_driven = entries.iter().filter(|e| e.upgrade_driven).count();
            let carried_noul = entries.iter().filter(|e| e.carried_noul).count();
            println!(
                "{cat:>28}: {} names | allow {} flag {} block {} unchecked {} | block-mass min/mean/sd {min:.3}/{mean:.3}/{sd:.3} | upgrade-driven flags (label=allow) {upgrade_driven} | carried qualifying noul {carried_noul}",
                entries.len(),
                count(Decision::Allow),
                count(Decision::Flag),
                count(Decision::Block),
                count(Decision::Unchecked),
            );
        }

        // Hard requirement: benign and edgy-but-fine names must never block.
        for cat in ["benign", "edgy-but-fine"] {
            if let Some(entries) = per_category.get(cat) {
                for entry in entries {
                    assert_ne!(
                        entry.decision,
                        Decision::Block,
                        "{cat} name {:?} was blocked — raise MODERATION_BLOCK_THRESHOLD",
                        entry.name
                    );
                }
            }
        }

        fn stats(values: &[f64]) -> (f64, f64, f64) {
            if values.is_empty() {
                return (f64::NAN, f64::NAN, f64::NAN);
            }
            let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
            (min, mean, var.sqrt())
        }
    }
}
