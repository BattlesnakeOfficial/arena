//! Rough relative-time labels ("2 hours ago", "in 5 minutes", "now").
//!
//! Same wording and thresholds as `chrono-humanize`'s default (rough)
//! display, which this replaced when arena moved from chrono to jiff.

use jiff::Timestamp;

/// `at` relative to `now`: within 10 seconds is "now", otherwise
/// "<period> ago" or "in <period>".
pub fn human_time(at: Timestamp, now: Timestamp) -> String {
    let secs = at.duration_since(now).as_secs();
    if secs.abs() <= 10 {
        return "now".to_string();
    }
    let period = rough_period(secs.abs());
    if secs > 0 {
        format!("in {period}")
    } else {
        format!("{period} ago")
    }
}

fn rough_period(n: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let plural = |count: i64, unit: &str| format!("{count} {unit}s");
    match n {
        n if n > 547 * DAY => plural((n / YEAR).max(2), "year"),
        n if n > 345 * DAY => "a year".to_string(),
        n if n > 45 * DAY => plural((n / MONTH).max(2), "month"),
        n if n > 29 * DAY => "a month".to_string(),
        n if n > 10 * DAY + 12 * HOUR => plural((n / WEEK).max(2), "week"),
        n if n > 6 * DAY + 12 * HOUR => "a week".to_string(),
        n if n > 36 * HOUR => plural((n / DAY).max(2), "day"),
        n if n > 22 * HOUR => "a day".to_string(),
        n if n > 90 * MINUTE => plural((n / HOUR).max(2), "hour"),
        n if n > 45 * MINUTE => "an hour".to_string(),
        n if n > 90 => plural((n / MINUTE).max(2), "minute"),
        n if n > 45 => "a minute".to_string(),
        n => plural(n, "second"),
    }
}

#[cfg(test)]
mod tests {
    use super::human_time;
    use jiff::{SignedDuration, Timestamp};

    fn ago(secs: i64) -> String {
        let now: Timestamp = "2026-11-18T12:00:00Z".parse().unwrap();
        human_time(now - SignedDuration::from_secs(secs), now)
    }

    #[test]
    fn matches_chrono_humanize_rough_wording() {
        assert_eq!(ago(0), "now");
        assert_eq!(ago(10), "now");
        assert_eq!(ago(11), "11 seconds ago");
        assert_eq!(ago(46), "a minute ago");
        assert_eq!(ago(91), "2 minutes ago");
        assert_eq!(ago(30 * 60), "30 minutes ago");
        assert_eq!(ago(46 * 60), "an hour ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(23 * 3600), "a day ago");
        assert_eq!(ago(3 * 86400), "3 days ago");
        assert_eq!(ago(7 * 86400), "a week ago");
        assert_eq!(ago(14 * 86400), "2 weeks ago");
        assert_eq!(ago(30 * 86400), "a month ago");
        assert_eq!(ago(90 * 86400), "3 months ago");
        assert_eq!(ago(360 * 86400), "a year ago");
        assert_eq!(ago(800 * 86400), "2 years ago");
        assert_eq!(ago(-120), "in 2 minutes");
    }
}
