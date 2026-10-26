//! Redaction helpers.
//!
//! Snake URLs can carry auth in the query string (`?token=…` is seen in
//! production), and hyper/reqwest error strings embed URLs. Everything that
//! reaches a log line, a metrics label, or an Eyes-bound span field goes
//! through these helpers first. The auth headers themselves are never logged.

/// Reduce a snake URL (or bare `host[:port]`) to `scheme://host[:port]` /
/// `host[:port]` — path, query and fragment are dropped so `?token=…` never
/// survives, and userinfo (`user:pass@`) is stripped so credentials never
/// reach a log line (PR #277 round 1).
pub fn redact_host_or_url(s: &str) -> String {
    if let Ok(u) = url::Url::parse(s)
        && let Some(host) = u.host_str().filter(|h| !h.is_empty())
    {
        return match u.port() {
            Some(p) => format!("{}://{host}:{p}", u.scheme()),
            None => format!("{}://{host}", u.scheme()),
        };
    }
    // Not a parseable URL with a host (a bare authority like
    // `user:pass@host:port` parses as an opaque URL whose host is None, so
    // it lands here): treat as host[:port], cut at the first byte that
    // begins path/query/fragment material, then drop any userinfo up to the
    // last `@` — the leftover must be safe on ANY input.
    let cut = s.find(['/', '?', '#']).unwrap_or(s.len());
    let authority = &s[..cut];
    match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    }
    .to_string()
}

/// Cut any URL-bearing token in an error string at its first `?` so query
/// strings (which can carry `?token=`) are dropped, and strip userinfo
/// (`user:pass@`) from the authority. Non-URL tokens pass through
/// untouched; whitespace structure is preserved (single spaces).
pub fn redact_error_string(s: &str) -> String {
    if !s.contains("://") {
        return s.to_string();
    }
    s.split_whitespace()
        .map(|tok| match tok.find("://") {
            Some(i) => {
                let (scheme, rest) = tok.split_at(i + 3);
                // Authority runs to the first path/query byte; the remainder
                // (path) is cut at its first `?`/`#` so query strings never
                // survive.
                let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                let (authority, path) = rest.split_at(auth_end);
                let authority = match authority.rfind('@') {
                    Some(at) => &authority[at + 1..],
                    None => authority,
                };
                let path_cut = path.find(['?', '#']).unwrap_or(path.len());
                format!("{scheme}{authority}{}", &path[..path_cut])
            }
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
    fn redact_host_or_url_strips_userinfo() {
        // A bare authority with userinfo parses as an opaque URL (host None),
        // so the fallback path handles it — credentials must not survive.
        assert_eq!(redact_host_or_url("user:SECRET@h.io:8080/x"), "h.io:8080");
        assert_eq!(redact_host_or_url("user:SECRET@host:9?token=x"), "host:9");
        assert_eq!(
            redact_host_or_url("u:p@snake.example.com"),
            "snake.example.com"
        );
        // Full URLs already go through `Url::host_str()`, which never carries
        // userinfo — but assert it so the guarantee is pinned on both paths.
        assert_eq!(
            redact_host_or_url("https://user:SECRET@snake.io:9/move?token=z"),
            "https://snake.io:9"
        );
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

    #[test]
    fn redact_error_string_strips_userinfo_from_urls() {
        let out = redact_error_string("error connecting to http://u:SECRET@snake.io/move failed");
        assert_eq!(out, "error connecting to http://snake.io/move failed");
        let out = redact_error_string("https://u:SECRET@snake.io:9/move?token=z down");
        assert_eq!(out, "https://snake.io:9/move down");
    }
}
