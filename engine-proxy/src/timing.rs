//! Per-request timing breakdown, formatted exactly like the Go proxy's
//! `HTTPTrace.String()` (`src/client/http.go`).
//!
//! ```text
//! dns=%.3f connect=%.3f tls=%.3f client_headers=%.3f client_body=%.3f first_byte=%.3f total=%.3f
//! ```
//!
//! Field semantics (Go parity):
//! - `dns`/`connect`/`tls`: connector-measured phase durations in
//!   milliseconds. Zero on every pooled reuse of a connection — the values
//!   are only reported for the request that first uses a fresh connection.
//! - `client_headers`/`client_body`: when the proxy finished writing the
//!   outbound request headers/body, measured from request start. We
//!   approximate the callbacks Go gets from `httptrace` with timestamps
//!   taken inside the outbound body wrapper: `client_headers` timestamps the
//!   first poll of the body's first frame (hyper writes headers before
//!   polling body frames, so this upper-bounds headers-written), and
//!   `client_body` timestamps the final frame's completed poll. Near-zero
//!   for the small buffered JSON bodies arena sends.
//! - `first_byte`/`total`: elapsed at response headers (the values are
//!   emitted with the headers, so `total` == `first_byte` here; Go computes
//!   its `Latency` at the same point for the timing header).

/// The seven-phase timing breakdown reported in `X-Battlesnake-Timing-Ms`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    pub dns_ms: f32,
    pub connect_ms: f32,
    pub tls_ms: f32,
    pub client_headers_ms: f32,
    pub client_body_ms: f32,
    pub first_byte_ms: f32,
    pub total_ms: f32,
}

/// Format exactly like Go's `HTTPTrace::String()`.
pub fn format_go_timing(t: &Timings) -> String {
    format!(
        "dns={:.3} connect={:.3} tls={:.3} client_headers={:.3} client_body={:.3} first_byte={:.3} total={:.3}",
        t.dns_ms,
        t.connect_ms,
        t.tls_ms,
        t.client_headers_ms,
        t.client_body_ms,
        t.first_byte_ms,
        t.total_ms
    )
}

/// Parse the timing string back (field order and count only — floats are
/// compared loosely by callers). Used by tests that pin the format.
pub fn parse_go_timing(s: &str) -> Option<Timings> {
    let mut t = Timings::default();
    for part in s.split_whitespace() {
        let (k, v) = part.split_once('=')?;
        let v: f32 = v.parse().ok()?;
        match k {
            "dns" => t.dns_ms = v,
            "connect" => t.connect_ms = v,
            "tls" => t.tls_ms = v,
            "client_headers" => t.client_headers_ms = v,
            "client_body" => t.client_body_ms = v,
            "first_byte" => t.first_byte_ms = v,
            "total" => t.total_ms = v,
            _ => return None,
        }
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_string_matches_go_format() {
        let t = Timings {
            dns_ms: 1.25,
            connect_ms: 12.5,
            tls_ms: 25.0,
            client_headers_ms: 0.1,
            client_body_ms: 0.2,
            first_byte_ms: 62.75,
            total_ms: 62.75,
        };
        let s = format_go_timing(&t);
        assert_eq!(
            s,
            "dns=1.250 connect=12.500 tls=25.000 client_headers=0.100 client_body=0.200 first_byte=62.750 total=62.750"
        );
    }

    #[test]
    fn timing_string_parses_back() {
        let s = "dns=0.000 connect=0.000 tls=0.000 client_headers=0.050 client_body=0.100 first_byte=31.900 total=31.900";
        let t = parse_go_timing(s).expect("parses");
        assert!((t.first_byte_ms - 31.9).abs() < 1e-3);
        assert_eq!(t.dns_ms, 0.0);
        assert!((t.client_headers_ms - 0.05).abs() < 1e-4);
    }

    #[test]
    fn all_zero_timing_renders_zeros() {
        let s = format_go_timing(&Timings::default());
        assert_eq!(
            s,
            "dns=0.000 connect=0.000 tls=0.000 client_headers=0.000 client_body=0.000 first_byte=0.000 total=0.000"
        );
        assert!(parse_go_timing(&s).is_some());
    }
}
