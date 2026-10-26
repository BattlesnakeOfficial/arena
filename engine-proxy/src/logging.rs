//! Redaction helpers.
//!
//! Snake URLs can carry auth in the query string (`?token=…` is seen in
//! production), and hyper/reqwest error strings embed URLs. Everything that
//! reaches a log line, a metrics label, or an Eyes-bound span field goes
//! through these helpers first. The auth headers themselves are never logged.

/// Reduce a snake URL (or bare `host[:port]`) to `scheme://host[:port]` /
/// `host[:port]` — path, query and fragment are dropped so `?token=…` never
/// survives.
pub fn redact_host_or_url(s: &str) -> String {
    if let Ok(u) = url::Url::parse(s)
        && let Some(host) = u.host_str().filter(|h| !h.is_empty())
    {
        return match u.port() {
            Some(p) => format!("{}://{host}:{p}", u.scheme()),
            None => format!("{}://{host}", u.scheme()),
        };
    }
    // Not a parseable URL (or scheme-only garbage like `host:9?x`): treat as
    // host[:port] and cut at the first byte that begins path/query/fragment
    // material.
    let cut = s.find(['/', '?', '#']).unwrap_or(s.len());
    s[..cut].to_string()
}

/// Cut any URL-bearing token in an error string at its first `?` so query
/// strings (which can carry `?token=`) are dropped. Non-URL tokens pass
/// through untouched; whitespace structure is preserved (single spaces).
pub fn redact_error_string(s: &str) -> String {
    if !s.contains("://") {
        return s.to_string();
    }
    s.split_whitespace()
        .map(|tok| match tok.find("://").map(|i| &tok[i..]) {
            Some(url_part) => match url_part.find('?') {
                Some(q) => format!("{}{}", &tok[..tok.len() - url_part.len()], &url_part[..q]),
                None => tok.to_string(),
            },
            None => tok.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_host_or_url_strips_query() {
        assert_eq!(
            redact_host_or_url("https://snake.example.com/move?token=abc&x=1"),
            "https://snake.example.com"
        );
        assert_eq!(
            redact_host_or_url("http://h.io:8080/a/b"),
            "http://h.io:8080"
        );
        assert_eq!(
            redact_host_or_url("https://snake.example.com?token=abc"),
            "https://snake.example.com"
        );
    }

    #[test]
    fn redact_host_or_url_bare_hosts() {
        assert_eq!(redact_host_or_url("snake.example.com"), "snake.example.com");
        assert_eq!(redact_host_or_url("127.0.0.1:8080/"), "127.0.0.1:8080");
        assert_eq!(redact_host_or_url("host:9?token=x"), "host:9");
    }

    #[test]
    fn redact_host_or_url_ipv6() {
        assert_eq!(
            redact_host_or_url("http://[::1]:9000/move?token=z"),
            "http://[::1]:9000"
        );
    }

    #[test]
    fn redact_error_string_cuts_query_on_url_tokens() {
        let err = "error trying to connect: https://snake.io/move?token=SECRET failed";
        let out = redact_error_string(err);
        assert!(!out.contains("SECRET"), "{out}");
        assert!(out.contains("https://snake.io/move"), "{out}");
    }

    #[test]
    fn redact_error_string_leaves_plain_strings_alone() {
        let s = "connection closed before message completed";
        assert_eq!(redact_error_string(s), s);
    }

    #[test]
    fn redact_error_string_handles_multiple_urls() {
        let out = redact_error_string("dial https://a.io/x?t=1 then https://b.io/y?token=q failed");
        assert_eq!(out, "dial https://a.io/x then https://b.io/y failed");
    }
}
