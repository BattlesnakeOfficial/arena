//! CLI output formatting utilities for human-readable and JSON output modes.

use chrono::{DateTime, Utc};
use chrono_humanize::HumanTime;
use colored::Colorize;
use is_terminal::IsTerminal;
use std::io;
use tabled::{builder::Builder, settings::Style};
use uuid::Uuid;

/// Output format for CLI commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable output with tables, colors, and formatting.
    Human,
    /// Machine-readable JSON output.
    Json,
}

impl OutputFormat {
    /// Determine the output format based on CLI flag and TTY detection.
    ///
    /// - If `format` is Some("json"), return Json
    /// - If `format` is Some("human"), return Human
    /// - If `format` is None, auto-detect based on stdout being a TTY
    pub fn from_flag(format: Option<&str>) -> Result<Self, String> {
        match format {
            Some("json") => Ok(OutputFormat::Json),
            Some("human") => Ok(OutputFormat::Human),
            Some(other) => Err(format!(
                "Invalid format '{}'. Use 'json' or 'human'.",
                other
            )),
            None => {
                if io::stdout().is_terminal() {
                    Ok(OutputFormat::Human)
                } else {
                    Ok(OutputFormat::Json)
                }
            }
        }
    }
}

/// Format a timestamp for human-readable output.
pub fn format_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Format a timestamp as relative time (e.g., "2 minutes ago").
pub fn format_relative_time(dt: DateTime<Utc>) -> String {
    HumanTime::from(dt).to_string()
}

/// Format a UUID, optionally truncated for display.
pub fn format_uuid_short(id: Uuid) -> String {
    let s = id.to_string();
    if s.len() > 8 {
        format!("{}...", &s[..8])
    } else {
        s
    }
}

/// Apply color to a status string based on its value.
pub fn status_colored(status: &str) -> String {
    match status.to_lowercase().as_str() {
        "complete" | "finished" | "done" | "public" => status.green().to_string(),
        "running" | "active" | "in_progress" => status.yellow().to_string(),
        "waiting" | "pending" | "private" => status.dimmed().to_string(),
        "error" | "failed" => status.red().to_string(),
        _ => status.to_string(),
    }
}

/// Build and print a table from headers and rows.
pub fn print_table(headers: Vec<&str>, rows: Vec<Vec<String>>) {
    let mut builder = Builder::default();
    builder.push_record(headers);
    for row in rows {
        builder.push_record(row);
    }
    let mut table = builder.build();
    table.with(Style::rounded());
    println!("{table}");
}

/// Print a key-value pair with proper formatting.
pub fn print_field(label: &str, value: &str) {
    println!("{}: {}", label.bold(), value);
}

/// Print a success message.
pub fn print_success(message: &str) {
    println!("{}", message.green());
}

/// Print an error as JSON to stderr for machine consumption.
pub fn print_json_error(message: &str) {
    eprintln!(
        "{}",
        serde_json::json!({
            "error": message
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_from_flag_json() {
        assert_eq!(
            OutputFormat::from_flag(Some("json")).unwrap(),
            OutputFormat::Json
        );
    }

    #[test]
    fn test_format_from_flag_human() {
        assert_eq!(
            OutputFormat::from_flag(Some("human")).unwrap(),
            OutputFormat::Human
        );
    }

    #[test]
    fn test_format_from_flag_invalid() {
        assert!(OutputFormat::from_flag(Some("xml")).is_err());
    }

    #[test]
    fn test_format_uuid_short() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        assert_eq!(format_uuid_short(uuid), "12345678...");
    }

    #[test]
    fn test_status_colored_public() {
        // Just verify it doesn't panic - actual color depends on terminal
        let result = status_colored("public");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_timestamp() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 1, 27, 12, 0, 0).unwrap();
        assert_eq!(format_timestamp(dt), "2026-01-27 12:00:00 UTC");
    }
}
