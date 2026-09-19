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

/// Format a byte count with a binary scale suffix (B / KB / MB / GB / TB).
///
/// Used across the flow/talker/bandwidth tables so large transfers read as
/// "1.4 MB" instead of "1468006". Binary (1024) steps, matching the netflow and
/// SNMP interface views.
pub fn format_bytes(bytes: f64) -> String {
    const TB: f64 = 1_099_511_627_776.0;
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1024.0;
    if bytes >= TB {
        format!("{:.1} TB", bytes / TB)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Format a byte-per-second rate as `"<bytes>/s"` (e.g. "1.4 MB/s").
pub fn format_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

/// Format an integer count with a decimal scale suffix (K / M / B).
///
/// Keeps small counts exact ("942") and scales large ones ("1.2M") so packet /
/// flow columns stay narrow and scannable.
pub fn format_count(count: u64) -> String {
    if count >= 1_000_000_000 {
        format!("{:.1}B", count as f64 / 1_000_000_000.0)
    } else if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
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

/// The **one** wall-clock formatter (#1123).
///
/// `YYYY-MM-DD HH:MM:SS` in the viewer's local zone, with the UTC offset —
/// `2026-09-19 15:42:10 +02:00`.
///
/// There were three, and they disagreed. The top bar hand-rolled
/// `(secs / 3600) % 24` — UTC, with **no suffix**; the systemd detail used
/// `chrono::Local`; the chart range was UTC and said so. An operator in UTC+2
/// read "as of 13:42" in the top bar and "15:42:10" on the unit that had just
/// restarted, and concluded the feed was two hours behind.
///
/// **Local with the offset, not UTC**, because the question an operator is
/// answering is "was that before or after I did the thing", and they know when
/// they did the thing in their own zone. The offset is shown so a screenshot
/// pasted into a ticket is still unambiguous.
#[must_use]
pub fn format_wall_clock(timestamp_ms: i64) -> String {
    use chrono::TimeZone;
    let secs = timestamp_ms.div_euclid(1000);
    match chrono::Local.timestamp_opt(secs, 0) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S %:z").to_string(),
        // An epoch a timezone database cannot place is shown as the number it
        // is, rather than as some other instant.
        _ => timestamp_ms.to_string(),
    }
}

/// [`format_wall_clock`] without the date — `HH:MM:SS +02:00`, for the top
/// bar's "as of", where the date is almost always today and the width is
/// scarce (#1123).
///
/// The offset stays. It is the whole point: a bare `13:42:10` is what made
/// three formatters indistinguishable from one broken feed.
#[must_use]
pub fn format_clock(timestamp_ms: i64) -> String {
    use chrono::TimeZone;
    let secs = timestamp_ms.div_euclid(1000);
    match chrono::Local.timestamp_opt(secs, 0) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%H:%M:%S %:z").to_string(),
        _ => timestamp_ms.to_string(),
    }
}

/// Format a time offset for chart axis labels.
///
/// Returns strings like "now", "-30s", "-5m", "-1h", "-2d".
pub fn format_time_offset(offset_ms: i64) -> String {
    if offset_ms == 0 {
        "now".to_string()
    } else if offset_ms < 60_000 {
        format!("-{}s", offset_ms / 1000)
    } else if offset_ms < 3_600_000 {
        format!("-{}m", offset_ms / 60_000)
    } else if offset_ms < 86_400_000 {
        format!("-{}h", offset_ms / 3_600_000)
    } else {
        format!("-{}d", offset_ms / 86_400_000)
    }
}

#[cfg(test)]
mod tests {

    /// **One formatter, and it says which zone it is in** (#1123).
    ///
    /// There were three and they disagreed: the top bar hand-rolled
    /// `(secs / 3600) % 24` — UTC with **no suffix** — the systemd detail used
    /// `chrono::Local`, and the chart range was UTC and said so. An operator in
    /// UTC+2 read "as of 13:42" in the top bar and "15:42:10" on the unit that
    /// had just restarted, and concluded the feed was two hours behind.
    ///
    /// Asserted as a shape, not a literal: the output is the viewer's local
    /// zone, and a test that pinned a string would pass only on a UTC machine
    /// — which is the assumption that produced three disagreeing formatters in
    /// the first place.
    #[test]
    fn the_wall_clock_carries_its_offset() {
        let ts = 1_700_000_000_000;

        let full = format_wall_clock(ts);
        let (date_time, offset) = full.rsplit_once(' ').expect("an offset suffix");
        assert_eq!(
            date_time.len(),
            19,
            "YYYY-MM-DD HH:MM:SS, got {date_time:?}"
        );
        assert!(
            (offset.starts_with('+') || offset.starts_with('-')) && offset.contains(':'),
            "signed, separated offset, got {offset:?}"
        );

        let short = format_clock(ts);
        let (time, short_offset) = short.rsplit_once(' ').expect("an offset suffix");
        assert_eq!(time.len(), 8, "HH:MM:SS, got {time:?}");
        assert_eq!(
            short_offset, offset,
            "both spellings agree about the zone: {full} vs {short}"
        );
        assert!(
            full.ends_with(&short),
            "the short form is the tail of the long one: {full} vs {short}"
        );
    }

    /// An hour on the clock is an hour of epoch, in any zone.
    #[test]
    fn an_hour_apart_reads_an_hour_apart() {
        let a = format_clock(1_700_000_000_000);
        let b = format_clock(1_700_000_000_000 + 3_600_000);
        assert_ne!(a, b);
        assert_eq!(&a[3..], &b[3..], "only the hour moves: {a} vs {b}");
    }

    use super::*;

    #[test]
    fn test_format_value() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(1.23456), "1.23");
        assert_eq!(format_value(1500.0), "1.5K");
        assert_eq!(format_value(2500000.0), "2.5M");
        assert_eq!(format_value(-1500.0), "-1.5K");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1536.0), "1.5 KB");
        assert_eq!(format_bytes(1_048_576.0), "1.0 MB");
        assert_eq!(format_bytes(1_610_612_736.0), "1.5 GB");
        assert_eq!(format_rate(2_097_152.0), "2.0 MB/s");
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(942), "942");
        assert_eq!(format_count(1_500), "1.5K");
        assert_eq!(format_count(2_500_000), "2.5M");
        assert_eq!(format_count(3_200_000_000), "3.2B");
    }

    #[test]
    fn test_format_time_offset() {
        assert_eq!(format_time_offset(0), "now");
        assert_eq!(format_time_offset(30_000), "-30s");
        assert_eq!(format_time_offset(300_000), "-5m");
        assert_eq!(format_time_offset(3_600_000), "-1h");
        assert_eq!(format_time_offset(86_400_000), "-1d");
        assert_eq!(format_time_offset(172_800_000), "-2d");
    }
}
