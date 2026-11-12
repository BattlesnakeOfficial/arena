//! Shared HTTP-boundary helpers used by both the server's snake client and
//! the battlesnake CLI's `play` subcommand.
//!
//! Async (they sit on `reqwest`'s response streaming), but they contain no
//! client construction, proxy routing, or logging — those stay with each
//! caller. There is no HTTP *client* in this crate; see lib.rs.

/// Read a response body chunk by chunk, stopping at `cap` bytes.
///
/// Once the cap is reached the remainder is discarded (we stop reading rather
/// than draining the connection) and the body is marked as truncated, so a
/// hostile server streaming hundreds of megabytes cannot exhaust memory.
pub async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<String, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    let mut capped = false;
    while let Some(chunk) = response.chunk().await? {
        if accumulate_body_chunk(&mut buf, &chunk, cap) {
            capped = true;
            break;
        }
    }
    let mut body = String::from_utf8_lossy(&buf).into_owned();
    if capped {
        body.push_str("… [body truncated at read cap]");
    }
    Ok(body)
}

/// Append `chunk` to `buf` without letting `buf` grow past `cap` bytes.
///
/// Returns `true` once the cap is reached, meaning the caller must stop
/// reading and discard any remaining input.
pub fn accumulate_body_chunk(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let remaining = cap.saturating_sub(buf.len());
    buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    buf.len() >= cap
}

/// Build a URL for a snake endpoint, properly handling query parameters
///
/// This appends the endpoint path (e.g., "move", "start", "end") to the base URL
/// while preserving any query parameters in the correct position.
///
/// Shared by the server's snake client, the on-demand snake health check
/// (`snake_health`), and the battlesnake CLI's `play` so every call hits
/// exactly the same URLs.
pub fn build_endpoint_url(base_url: &str, endpoint: &str) -> String {
    // Try to parse as a proper URL
    if let Ok(mut url) = url::Url::parse(base_url) {
        // Get the current path, trim trailing slashes, and append the endpoint
        let current_path = url.path().trim_end_matches('/');
        let new_path = format!("{}/{}", current_path, endpoint);
        url.set_path(&new_path);
        url.to_string()
    } else {
        // Fallback to simple string concatenation if URL parsing fails
        format!("{}/{}", base_url.trim_end_matches('/'), endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BODY_READ_CAP_BYTES: usize = 64 * 1024;

    #[test]
    fn body_chunks_under_cap_accumulate_losslessly() {
        let mut buf = Vec::new();
        assert!(!accumulate_body_chunk(&mut buf, b"hello ", 64));
        assert!(!accumulate_body_chunk(&mut buf, b"world", 64));
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn body_chunk_exceeding_cap_is_cut_at_cap() {
        let mut buf = Vec::new();
        let big = vec![b'x'; BODY_READ_CAP_BYTES * 3];
        assert!(accumulate_body_chunk(&mut buf, &big, BODY_READ_CAP_BYTES));
        assert_eq!(buf.len(), BODY_READ_CAP_BYTES);
    }

    #[test]
    fn body_chunk_landing_exactly_on_cap_stops_reading() {
        let mut buf = vec![b'a'; 10];
        assert!(accumulate_body_chunk(&mut buf, b"bbbbbb", 16));
        assert_eq!(buf.len(), 16);
        assert!(accumulate_body_chunk(&mut buf, b"ccc", 16));
        assert_eq!(buf.len(), 16);
    }

    #[test]
    fn test_build_endpoint_url_simple() {
        let url = build_endpoint_url("https://example.com", "move");
        assert_eq!(url, "https://example.com/move");
    }

    #[test]
    fn test_build_endpoint_url_with_trailing_slash() {
        let url = build_endpoint_url("https://example.com/", "move");
        assert_eq!(url, "https://example.com/move");
    }

    #[test]
    fn test_build_endpoint_url_with_path() {
        let url = build_endpoint_url("https://example.com/api/v1", "move");
        assert_eq!(url, "https://example.com/api/v1/move");
    }

    #[test]
    fn test_build_endpoint_url_with_query_params() {
        let url = build_endpoint_url("https://example.com?token=secret", "move");
        assert_eq!(url, "https://example.com/move?token=secret");
    }

    #[test]
    fn test_build_endpoint_url_with_path_and_query_params() {
        let url = build_endpoint_url("https://example.com/api?token=secret&version=2", "move");
        assert_eq!(url, "https://example.com/api/move?token=secret&version=2");
    }

    #[test]
    fn test_build_endpoint_url_with_trailing_slash_and_query_params() {
        let url = build_endpoint_url("https://example.com/api/?token=secret", "start");
        assert_eq!(url, "https://example.com/api/start?token=secret");
    }

    #[test]
    fn test_build_endpoint_url_all_endpoints() {
        let base = "https://snake.example.com?auth=abc123";
        assert_eq!(
            build_endpoint_url(base, "move"),
            "https://snake.example.com/move?auth=abc123"
        );
        assert_eq!(
            build_endpoint_url(base, "start"),
            "https://snake.example.com/start?auth=abc123"
        );
        assert_eq!(
            build_endpoint_url(base, "end"),
            "https://snake.example.com/end?auth=abc123"
        );
    }

    /// Strategy that generates a valid base URL with optional path and query params
    fn arb_base_url() -> impl Strategy<Value = String> {
        let scheme = prop_oneof![Just("http"), Just("https")];
        let host = prop_oneof![
            Just("example.com".to_string()),
            Just("snake.io".to_string()),
            Just("localhost:8080".to_string()),
            Just("192.168.1.1:3000".to_string()),
        ];
        let path = prop_oneof![
            Just("".to_string()),
            Just("/".to_string()),
            Just("/api".to_string()),
            Just("/api/v1".to_string()),
            Just("/api/v1/".to_string()),
            Just("/snakes/my-snake".to_string()),
        ];
        let query = prop_oneof![
            Just("".to_string()),
            Just("?token=abc".to_string()),
            Just("?token=abc&version=2".to_string()),
            Just("?auth=secret123".to_string()),
        ];
        (scheme, host, path, query).prop_map(|(s, h, p, q)| format!("{}://{}{}{}", s, h, p, q))
    }

    /// Strategy for the three battlesnake endpoints
    fn arb_endpoint() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("move"), Just("start"), Just("end"),]
    }

    proptest! {
        // -- build_endpoint_url properties --

        #[test]
        fn prop_endpoint_url_is_parseable(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            prop_assert!(url::Url::parse(&result).is_ok(),
                "Result '{}' should be a valid URL", result);
        }

        #[test]
        fn prop_endpoint_url_preserves_scheme(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = url::Url::parse(&base).unwrap();
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.scheme(), result_parsed.scheme());
        }

        #[test]
        fn prop_endpoint_url_preserves_host(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = url::Url::parse(&base).unwrap();
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.host_str(), result_parsed.host_str());
        }

        #[test]
        fn prop_endpoint_url_preserves_query(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = url::Url::parse(&base).unwrap();
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.query(), result_parsed.query());
        }

        #[test]
        fn prop_endpoint_url_contains_endpoint(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert!(result_parsed.path().ends_with(&format!("/{}", endpoint)),
                "Path '{}' should end with '/{}'", result_parsed.path(), endpoint);
        }

        #[test]
        fn prop_endpoint_url_no_double_slashes(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert!(!result_parsed.path().contains("//"),
                "Path '{}' should not contain double slashes", result_parsed.path());
        }

        #[test]
        fn prop_endpoint_url_preserves_port(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = url::Url::parse(&base).unwrap();
            let result_parsed = url::Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.port(), result_parsed.port());
        }
    }
}
