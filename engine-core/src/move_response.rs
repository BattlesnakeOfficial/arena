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
    pub latency_ms: Option<i64>,
    pub timed_out: bool,
    pub shout: Option<String>,
    /// HTTP status the snake answered with; `None` when no response arrived
    /// (timeout, transport error). Surfaced in frames as `StatusCode` like
    /// the legacy engine. On `/move` an engine-proxy fault is retried
    /// directly, so the turn carries the direct answer's status; the fault
    /// itself still yields `None` here, as it does on the start/end/info and
    /// health paths, which keep the fallback.
    pub status_code: Option<u16>,
    /// The snake answered but the answer was unusable: a non-2xx status, a
    /// body that isn't a move response, or an unrecognized direction. Kept
    /// apart from `timed_out` (no answer at all). An engine-proxy fault
    /// never sets this (it is not the snake's answer); on `/move` such
    /// faults are retried directly, so the turn carries the direct answer's
    /// `errored` instead.
    pub errored: bool,
}

/// Parse a direction string into a Direction enum
///
/// Shared with the on-demand snake health check (`snake_health`) so test
/// results judge moves exactly like real games do.
pub fn parse_direction(s: &str) -> Option<Direction> {
    s.parse().ok()
}

/// Whether a parsed move response still counts as an error: real games use
/// the move anyway, but a non-2xx status or an unrecognized direction means
/// the snake's server is misbehaving.
pub fn move_errored(status: Option<u16>, direction: Option<Direction>) -> bool {
    direction.is_none() || !status.is_some_and(|s| (200..300).contains(&s))
}

/// Interpret a body the snake answered with. Ok only when the body parses
/// as a move response; the direction falls back to the last move, then Up.
/// Pure: logging stays with the HTTP caller.
pub fn interpret_move_response(
    snake_id: &str,
    body: &str,
    status_code: Option<u16>,
    latency_ms: Option<i64>,
    last_direction: Option<Direction>,
) -> std::result::Result<MoveResult, serde_json::Error> {
    let response: MoveResponse = serde_json::from_str(body)?;
    let parsed = parse_direction(&response.direction);
    Ok(MoveResult {
        snake_id: snake_id.to_string(),
        direction: parsed.unwrap_or_else(|| last_direction.unwrap_or(Direction::Up)),
        latency_ms,
        timed_out: false,
        shout: sanitize_shout(response.shout),
        status_code,
        errored: move_errored(status_code, parsed),
    })
}

/// The snake answered but the body was unusable: fallback move, errored.
pub fn parse_failure_result(
    snake_id: &str,
    last_direction: Option<Direction>,
    latency_ms: Option<i64>,
    status_code: Option<u16>,
) -> MoveResult {
    MoveResult {
        snake_id: snake_id.to_string(),
        direction: last_direction.unwrap_or(Direction::Up),
        latency_ms,
        timed_out: false,
        shout: None,
        status_code,
        errored: true,
    }
}

/// No answer arrived (timeout / transport failure): fallback move, timed_out.
pub fn transport_fallback_result(
    snake_id: &str,
    last_direction: Option<Direction>,
    latency_ms: Option<i64>,
    timed_out: bool,
) -> MoveResult {
    MoveResult {
        snake_id: snake_id.to_string(),
        direction: last_direction.unwrap_or(Direction::Up),
        latency_ms,
        timed_out,
        shout: None,
        status_code: None,
        errored: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_valid_body_uses_parsed_direction() {
        let result = interpret_move_response("s1", r#"{"move":"left"}"#, Some(200), Some(12), None)
            .expect("valid body parses");
        assert_eq!(result.snake_id, "s1");
        assert_eq!(result.direction, Direction::Left);
        assert_eq!(result.latency_ms, Some(12));
        assert!(!result.timed_out);
        assert_eq!(result.status_code, Some(200));
        assert!(!result.errored);
        assert_eq!(result.shout, None);
    }

    #[test]
    fn interpret_bad_direction_falls_back_and_errors() {
        let result = interpret_move_response(
            "s1",
            r#"{"move":"sideways"}"#,
            Some(200),
            Some(9),
            Some(Direction::Down),
        )
        .expect("body still parses as a move response");
        assert_eq!(result.direction, Direction::Down, "last move wins");
        assert!(result.errored, "unrecognized direction is an error");
        assert!(!result.timed_out);
    }

    #[test]
    fn interpret_unparseable_body_is_err() {
        let err =
            interpret_move_response("s1", "not json", Some(200), None, None).expect_err("bad body");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn parse_failure_result_is_errored_fallback() {
        let result = parse_failure_result("s1", Some(Direction::Down), Some(40), Some(500));
        assert_eq!(result.direction, Direction::Down);
        assert_eq!(result.latency_ms, Some(40));
        assert!(!result.timed_out);
        assert_eq!(result.status_code, Some(500));
        assert!(result.errored);
        assert_eq!(result.shout, None);
    }

    #[test]
    fn transport_fallback_result_is_timed_out_without_status() {
        let result = transport_fallback_result("s1", None, None, true);
        assert_eq!(result.direction, Direction::Up, "no last move: Up");
        assert!(result.timed_out);
        assert_eq!(result.status_code, None);
        assert!(!result.errored);
        assert_eq!(result.shout, None);
    }
}
