//! Shared formatting utilities for the ZenSight views.

/// Format a numeric value for display with appropriate scale suffix.
///
/// - Values >= 1M display as "X.XM"
/// - Values >= 1K display as "X.XK"
/// - Integer values display without decimal places
/// - Other values display with 2 decimal places
pub fn format_value(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value.abs() >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        format!("{:.2}", value)
    }
}

/// Format a Unix timestamp (milliseconds) as a relative time string.
///
/// Returns strings like "just now", "5s ago", "3m ago", "2h ago".
pub fn format_timestamp(timestamp_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let diff_ms = now - timestamp_ms;

    if diff_ms < 1000 {
        "just now".to_string()
    } else if diff_ms < 60_000 {
        format!("{}s ago", diff_ms / 1000)
    } else if diff_ms < 3_600_000 {
        format!("{}m ago", diff_ms / 60_000)
    } else {
        format!("{}h ago", diff_ms / 3_600_000)
    }
}

/// Format a time offset for chart axis labels.
///
/// Returns strings like "now", "-30s", "-5m", "-1h".
pub fn format_time_offset(offset_ms: i64) -> String {
    if offset_ms == 0 {
        "now".to_string()
    } else if offset_ms < 60_000 {
        format!("-{}s", offset_ms / 1000)
    } else if offset_ms < 3_600_000 {
        format!("-{}m", offset_ms / 60_000)
    } else {
        format!("-{}h", offset_ms / 3_600_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_value() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(3.14159), "3.14");
        assert_eq!(format_value(1500.0), "1.5K");
        assert_eq!(format_value(2500000.0), "2.5M");
        assert_eq!(format_value(-1500.0), "-1.5K");
    }

    #[test]
    fn test_format_time_offset() {
        assert_eq!(format_time_offset(0), "now");
        assert_eq!(format_time_offset(30_000), "-30s");
        assert_eq!(format_time_offset(300_000), "-5m");
        assert_eq!(format_time_offset(3_600_000), "-1h");
    }
}
