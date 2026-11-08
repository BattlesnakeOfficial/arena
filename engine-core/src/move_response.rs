//! Reading a snake's `/move` response: parsing the direction, sanitizing
//! the shout, and the fallback semantics when the answer is missing or
//! unusable. Pure — HTTP stays with the caller.

use rules::Direction;
use serde::Deserialize;

/// Cap on response bodies read from snakes (64 KiB). Beyond this the rest of
/// the body is discarded and marked truncated, so a hostile server streaming
/// endless bytes cannot exhaust memory.
pub const BODY_READ_CAP_BYTES: usize = 64 * 1024;

/// Maximum length of a shout kept after sanitization. Longer shouts are
/// truncated on a char boundary. 256 chars is far beyond any real shout
/// (the board viewer clips much shorter) while bounding what an arbitrary
/// third-party server can push into frame JSON (DEV-1297).
pub const MAX_SHOUT_CHARS: usize = 256;

/// Whitespace-like separators become a space rather than being dropped, so
/// word boundaries survive sanitization ("gg\nwp" → "gg wp", not "ggwp").
fn map_to_space(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{2028}' | '\u{2029}' => ' ',
        _ => c,
    }
}

/// Invisible / disguising characters, stripped outright:
/// - remaining Cc (C0 minus mapped whitespace, DEL, C1) via `is_control`
/// - Cf format characters: soft hyphen, Arabic letter mark, Khmer inherent
///   vowels, Mongolian vowel separator, ZWSP/ZWNJ, LRM/RLM, bidi embedding
///   and overrides (U+202A–U+202E — the RLO spoofing class), word joiner /
///   invisible separators / bidi isolates (U+2060–U+206F), BOM (U+FEFF),
///   interlinear annotation (U+FFF9–U+FFFB), and Unicode tags
///   (U+E0000–U+E007F — "ASCII smuggling": invisible to humans, readable
///   by models)
/// - invisible non-Cf: combining grapheme joiner, blank-rendering Hangul
///   fillers
///
/// Deliberately KEPT: U+200D (ZWJ) and U+FE00–U+FE0F (variation selectors)
/// — stripping them breaks multi-codepoint emoji (👨‍👩‍👧, ❤️).
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(u32::from(c),
            0x00AD | 0x034F | 0x061C
            | 0x115F | 0x1160 | 0x17B4 | 0x17B5
            | 0x180E
            | 0x200B | 0x200C | 0x200E | 0x200F
            | 0x202A..=0x202E | 0x2060..=0x206F
            | 0x3164 | 0xFFA0
            | 0xFEFF | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F)
}

/// Sanitize a shout from a move response: map separators to spaces, strip
/// invisible characters, collapse whitespace runs, trim, truncate to
/// [`MAX_SHOUT_CHARS`] on a char boundary (then trim again). Returns None
/// when nothing printable remains (frame code treats None as "no shout"
/// and serializes "").
pub fn sanitize_shout(raw: Option<String>) -> Option<String> {
    let s = raw?;
    let mut cleaned = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars().map(map_to_space) {
        if is_invisible(c) {
            continue;
        }
        if c == ' ' {
            if !last_was_space {
                cleaned.push(' ');
            }
            last_was_space = true;
        } else {
            cleaned.push(c);
            last_was_space = false;
        }
    }
    let mut cleaned = cleaned.trim().to_string();
    if cleaned.chars().count() > MAX_SHOUT_CHARS {
        cleaned = cleaned.chars().take(MAX_SHOUT_CHARS).collect();
        cleaned = cleaned.trim_end().to_string();
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Response from a snake's /move endpoint
#[derive(Debug, Deserialize)]
pub struct MoveResponse {
    #[serde(rename = "move")]
    pub direction: String,
    pub shout: Option<String>,
}

/// Result of a move request including timing info
#[derive(Debug, Clone)]
pub struct MoveResult {
    pub snake_id: String,
    pub direction: Direction,
    /// Measured from request start to body completion, or to the deadline
    /// firing on the direct path. `None` when nothing was measured: transport
    /// failures, and timeouts reported by the engine proxy or the CLI.
    pub latency_ms: Option<i64>,
    pub shout: Option<String>,
    /// How the request ended; the single classification source — readers
    /// must not re-derive outcomes from loose status/bool pairs.
    pub outcome: MoveOutcome,
}

/// How a `/move` request ended. The single classification source: readers
/// must not re-derive outcomes from loose status/bool pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    /// The snake answered 2xx with a usable, recognized move.
    Ok { status_code: u16 },
    /// No complete answer arrived within the move timeout — send or body
    /// phase. `latency_ms` is the real elapsed time on the direct path;
    /// proxied and CLI timeouts have `None`.
    TimedOut,
    /// Transport-level failure (connect refused/reset/DNS): no HTTP answer.
    Transport,
    /// The snake answered a non-2xx HTTP status.
    HttpError { status_code: u16 },
    /// An answer arrived but was unusable: unparseable/truncated body or
    /// unrecognized direction. Status is 2xx by construction (a non-2xx
    /// status classifies as `HttpError`).
    Invalid { status_code: u16 },
}

impl MoveOutcome {
    /// HTTP status when an answer arrived; `None` when none did.
    pub fn status_code(&self) -> Option<u16> {
        match *self {
            MoveOutcome::Ok { status_code }
            | MoveOutcome::HttpError { status_code }
            | MoveOutcome::Invalid { status_code } => Some(status_code),
            MoveOutcome::TimedOut | MoveOutcome::Transport => None,
        }
    }

    /// No answer arrived inside the move budget (`TimedOut` or `Transport`).
    /// Every legacy surface (DB `snake_turns.timed_out`, frame
    /// `TIMEOUT_ERROR`, `reported_latency_ms`) treats both as a timeout.
    pub fn timed_out(&self) -> bool {
        matches!(self, MoveOutcome::TimedOut | MoveOutcome::Transport)
    }

    /// The snake answered but the answer was unusable (`HttpError` or
    /// `Invalid`).
    pub fn errored(&self) -> bool {
        matches!(
            self,
            MoveOutcome::HttpError { .. } | MoveOutcome::Invalid { .. }
        )
    }
}

/// Parse a direction string into a Direction enum
///
/// Shared with the on-demand snake health check (`snake_health`) so test
/// results judge moves exactly like real games do.
pub fn parse_direction(s: &str) -> Option<Direction> {
    s.parse().ok()
}

/// Interpret a body the snake answered with. Ok only when the body parses
/// as a move response; the direction falls back to the last move, then Up.
/// Pure: logging stays with the HTTP caller.
pub fn interpret_move_response(
    snake_id: &str,
    body: &str,
    status_code: u16,
    latency_ms: Option<i64>,
    last_direction: Option<Direction>,
) -> std::result::Result<MoveResult, serde_json::Error> {
    let response: MoveResponse = serde_json::from_str(body)?;
    let parsed = parse_direction(&response.direction);
    let outcome = if !(200..300).contains(&status_code) {
        MoveOutcome::HttpError { status_code }
    } else if parsed.is_none() {
        MoveOutcome::Invalid { status_code }
    } else {
        MoveOutcome::Ok { status_code }
    };
    Ok(MoveResult {
        snake_id: snake_id.to_string(),
        direction: parsed.unwrap_or_else(|| last_direction.unwrap_or(Direction::Up)),
        latency_ms,
        shout: sanitize_shout(response.shout),
        outcome,
    })
}

/// The snake answered but the body was unusable: fallback move, errored.
/// A non-2xx status classifies as `HttpError`, anything else as `Invalid`.
pub fn parse_failure_result(
    snake_id: &str,
    last_direction: Option<Direction>,
    latency_ms: Option<i64>,
    status_code: u16,
) -> MoveResult {
    MoveResult {
        snake_id: snake_id.to_string(),
        direction: last_direction.unwrap_or(Direction::Up),
        latency_ms,
        shout: None,
        outcome: if (200..300).contains(&status_code) {
            MoveOutcome::Invalid { status_code }
        } else {
            MoveOutcome::HttpError { status_code }
        },
    }
}

/// No answer arrived (timeout / transport failure): fallback move.
/// Callers pass [`MoveOutcome::TimedOut`] (the deadline fired) or
/// [`MoveOutcome::Transport`] (a network failure); both count as a
/// timeout on every legacy surface. `latency_ms` is the real elapsed
/// time when known.
pub fn transport_fallback_result(
    snake_id: &str,
    last_direction: Option<Direction>,
    latency_ms: Option<i64>,
    outcome: MoveOutcome,
) -> MoveResult {
    MoveResult {
        snake_id: snake_id.to_string(),
        direction: last_direction.unwrap_or(Direction::Up),
        latency_ms,
        shout: None,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_valid_body_uses_parsed_direction() {
        let result = interpret_move_response("s1", r#"{"move":"left"}"#, 200, Some(12), None)
            .expect("valid body parses");
        assert_eq!(result.snake_id, "s1");
        assert_eq!(result.direction, Direction::Left);
        assert_eq!(result.latency_ms, Some(12));
        assert!(!result.outcome.timed_out());
        assert_eq!(result.outcome.status_code(), Some(200));
        assert!(!result.outcome.errored());
        assert_eq!(result.shout, None);
    }

    #[test]
    fn interpret_bad_direction_falls_back_and_errors() {
        let result = interpret_move_response(
            "s1",
            r#"{"move":"sideways"}"#,
            200,
            Some(9),
            Some(Direction::Down),
        )
        .expect("body still parses as a move response");
        assert_eq!(result.direction, Direction::Down, "last move wins");
        assert_eq!(result.outcome, MoveOutcome::Invalid { status_code: 200 });
        assert!(
            result.outcome.errored(),
            "unrecognized direction is an error"
        );
        assert!(!result.outcome.timed_out());
    }

    #[test]
    fn interpret_non_2xx_status_classifies_as_http_error() {
        let result = interpret_move_response(
            "s1",
            r#"{"move":"up"}"#,
            500,
            Some(30),
            Some(Direction::Down),
        )
        .expect("body still parses as a move response");
        assert_eq!(result.outcome, MoveOutcome::HttpError { status_code: 500 });
        assert!(result.outcome.errored());
        assert!(!result.outcome.timed_out());
    }

    #[test]
    fn interpret_unparseable_body_is_err() {
        let err = interpret_move_response("s1", "not json", 200, None, None).expect_err("bad body");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn parse_failure_result_is_errored_fallback() {
        let result = parse_failure_result("s1", Some(Direction::Down), Some(40), 500);
        assert_eq!(result.direction, Direction::Down);
        assert_eq!(result.latency_ms, Some(40));
        assert!(!result.outcome.timed_out());
        assert_eq!(result.outcome, MoveOutcome::HttpError { status_code: 500 });
        assert!(result.outcome.errored());
        assert_eq!(result.shout, None);

        let result = parse_failure_result("s1", Some(Direction::Down), Some(40), 200);
        assert_eq!(
            result.outcome,
            MoveOutcome::Invalid { status_code: 200 },
            "a 2xx unusable answer is Invalid, not HttpError"
        );
    }

    #[test]
    fn transport_fallback_result_is_timed_out_without_status() {
        for outcome in [MoveOutcome::TimedOut, MoveOutcome::Transport] {
            let result = transport_fallback_result("s1", None, Some(500), outcome);
            assert_eq!(result.direction, Direction::Up, "no last move: Up");
            assert!(result.outcome.timed_out());
            assert_eq!(result.outcome.status_code(), None);
            assert!(!result.outcome.errored());
            assert_eq!(result.shout, None);
        }
    }
}
