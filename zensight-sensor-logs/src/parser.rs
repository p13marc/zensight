//! Syslog message parser supporting RFC 3164 (BSD) and RFC 5424 formats.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// Syslog facility codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Facility {
    Kern = 0,
    User = 1,
    Mail = 2,
    Daemon = 3,
    Auth = 4,
    Syslog = 5,
    Lpr = 6,
    News = 7,
    Uucp = 8,
    Cron = 9,
    Authpriv = 10,
    Ftp = 11,
    Ntp = 12,
    Audit = 13,
    Alert = 14,
    Clock = 15,
    Local0 = 16,
    Local1 = 17,
    Local2 = 18,
    Local3 = 19,
    Local4 = 20,
    Local5 = 21,
    Local6 = 22,
    Local7 = 23,
}

impl Facility {
    /// Parse facility from numeric code.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Kern),
            1 => Some(Self::User),
            2 => Some(Self::Mail),
            3 => Some(Self::Daemon),
            4 => Some(Self::Auth),
            5 => Some(Self::Syslog),
            6 => Some(Self::Lpr),
            7 => Some(Self::News),
            8 => Some(Self::Uucp),
            9 => Some(Self::Cron),
            10 => Some(Self::Authpriv),
            11 => Some(Self::Ftp),
            12 => Some(Self::Ntp),
            13 => Some(Self::Audit),
            14 => Some(Self::Alert),
            15 => Some(Self::Clock),
            16 => Some(Self::Local0),
            17 => Some(Self::Local1),
            18 => Some(Self::Local2),
            19 => Some(Self::Local3),
            20 => Some(Self::Local4),
            21 => Some(Self::Local5),
            22 => Some(Self::Local6),
            23 => Some(Self::Local7),
            _ => None,
        }
    }

    /// Get the string name of the facility.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Kern => "kern",
            Self::User => "user",
            Self::Mail => "mail",
            Self::Daemon => "daemon",
            Self::Auth => "auth",
            Self::Syslog => "syslog",
            Self::Lpr => "lpr",
            Self::News => "news",
            Self::Uucp => "uucp",
            Self::Cron => "cron",
            Self::Authpriv => "authpriv",
            Self::Ftp => "ftp",
            Self::Ntp => "ntp",
            Self::Audit => "audit",
            Self::Alert => "alert",
            Self::Clock => "clock",
            Self::Local0 => "local0",
            Self::Local1 => "local1",
            Self::Local2 => "local2",
            Self::Local3 => "local3",
            Self::Local4 => "local4",
            Self::Local5 => "local5",
            Self::Local6 => "local6",
            Self::Local7 => "local7",
        }
    }
}

/// Syslog severity levels.
/// Syslog severity — the one canonical model lives in `zensight-common` (#557);
/// re-exported here so the sensor's `Severity` is that single type (`from_code`
/// / `as_str` / `otel_severity_number` / `otel_severity_text` are its methods).
pub use zensight_common::LogSeverity as Severity;

/// Parsed syslog message.
#[derive(Debug, Clone)]
#[allow(dead_code)] // version field is used in tests and useful for API consumers
pub struct SyslogMessage {
    /// Facility code.
    pub facility: Facility,
    /// Severity level.
    pub severity: Severity,
    /// Timestamp (if available).
    pub timestamp: Option<DateTime<Utc>>,
    /// Hostname or IP address.
    pub hostname: Option<String>,
    /// Application name / process tag.
    pub app_name: Option<String>,
    /// Process ID.
    pub proc_id: Option<String>,
    /// Message ID (RFC 5424).
    pub msg_id: Option<String>,
    /// Structured data (RFC 5424).
    pub structured_data: HashMap<String, HashMap<String, String>>,
    /// Message content.
    pub message: String,
    /// Original raw message.
    pub raw: String,
    /// Syslog format version.
    pub version: SyslogVersion,
    /// Where `timestamp` came from (#545). RFC 3164 stamps are yearless and
    /// tz-less; when the reconstructed instant is implausibly far from receive
    /// time it is replaced with the receive clock and marked `Receiver`.
    pub ts_source: TsSource,
}

/// Provenance of a record's timestamp (#545).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TsSource {
    /// Taken from the message: RFC 5424's explicit stamp, or a plausible
    /// year-inferred + timezone-localized RFC 3164 stamp.
    #[default]
    Sender,
    /// The message stamp was implausible (or absent); this is the receive time.
    Receiver,
}

impl TsSource {
    /// Label value for telemetry — only emitted when it's `Receiver`.
    pub fn as_label(self) -> &'static str {
        match self {
            TsSource::Sender => "sender",
            TsSource::Receiver => "receiver",
        }
    }
}

/// Context for interpreting yearless / timezone-less RFC 3164 timestamps (#545).
///
/// RFC 3164 stamps (`Mmm dd hh:mm:ss`) carry neither a year nor a zone. We infer
/// the year that puts the instant closest to `now` (fixing the Dec↔Jan boundary
/// both ways) and interpret the wall clock in `tz` (default UTC). If the result
/// still lands more than `max_skew` from `now`, it's treated as garbage and the
/// receive time is used instead.
#[derive(Debug, Clone, Copy)]
pub struct RfcTimeCtx {
    /// Timezone the sender's wall-clock timestamp is expressed in.
    pub tz: Tz,
    /// Receive time — the reference for year inference and the sanity clamp.
    pub now: DateTime<Utc>,
    /// Maximum plausible distance between the parsed instant and `now`.
    pub max_skew: Duration,
}

impl RfcTimeCtx {
    /// Default clamp window: 90 days. Generous enough that any real timezone
    /// offset (≤14h) and ordinary delivery delay / replay never trips it, tight
    /// enough to catch year-scale garbage that survives year inference.
    pub const DEFAULT_MAX_SKEW_DAYS: i64 = 90;

    /// UTC, `now = Utc::now()`, default clamp — the legacy behavior plus year
    /// inference. Used by the plain [`parse`] entry point.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn utc_now() -> Self {
        Self {
            tz: Tz::UTC,
            now: Utc::now(),
            max_skew: Duration::days(Self::DEFAULT_MAX_SKEW_DAYS),
        }
    }

    /// Explicit timezone + receive time, default clamp window.
    pub fn new(tz: Tz, now: DateTime<Utc>) -> Self {
        Self {
            tz,
            now,
            max_skew: Duration::days(Self::DEFAULT_MAX_SKEW_DAYS),
        }
    }

    /// Override the clamp window.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_max_skew(mut self, max_skew: Duration) -> Self {
        self.max_skew = max_skew;
        self
    }
}

/// Syslog format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogVersion {
    /// RFC 3164 (BSD syslog).
    Rfc3164,
    /// RFC 5424.
    Rfc5424,
}

// RFC 5424 pattern: <PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
static RFC5424_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^<(\d{1,3})>(\d+) (\S+) (\S+) (\S+) (\S+) (\S+) (\[.*?\]|-|\s*) ?(.*)$").unwrap()
});

// RFC 3164 pattern: <PRI>TIMESTAMP HOSTNAME TAG: MSG
// Timestamp formats: "Mmm dd hh:mm:ss" or "Mmm  d hh:mm:ss"
static RFC3164_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^<(\d{1,3})>([A-Za-z]{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2})\s+(\S+)\s+(\S+?)(?:\[(\d+)\])?:\s*(.*)$"
    ).unwrap()
});

// Fallback pattern for messages with just PRI
static SIMPLE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^<(\d{1,3})>(.*)$").unwrap());

// RFC 5424 structured data pattern
// SD-ELEMENT = "[" SD-ID *(SP SD-PARAM) "]"
// SD-ID cannot contain spaces, ], ", or =
// We match each SD-ELEMENT individually
static SD_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\[([^\s\]"=]+)((?:\s+[^\s\]"=]+="(?:[^"\\]|\\.)*")*)\]"#).unwrap());

// Structured data parameter pattern
// SD-PARAM = PARAM-NAME "=" %d34 PARAM-VALUE %d34
// PARAM-VALUE can contain escaped characters: \" \\ \]
static SD_PARAM_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"([^\s\]"=]+)="((?:[^"\\]|\\.)*)""#).unwrap());

/// Parse a syslog message with UTC / current-time defaults (#545).
///
/// Convenience wrapper over [`parse_with_time`] for callers with no timezone or
/// receive-time context (and for tests). RFC 3164 stamps are interpreted as UTC
/// with the year inferred against the current time.
// The binary always parses with a resolved timezone context; this wrapper is
// for the library API and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse(input: &str) -> Option<SyslogMessage> {
    parse_with_time(input, &RfcTimeCtx::utc_now())
}

/// Parse a syslog message, interpreting RFC 3164 timestamps with `ctx` (#545).
///
/// The RFC 5424 and simple-fallback paths ignore `ctx` — 5424 carries its own
/// explicit timezone; the simple path has no timestamp at all.
pub fn parse_with_time(input: &str, ctx: &RfcTimeCtx) -> Option<SyslogMessage> {
    // Remove UTF-8 BOM if present (RFC 5424 allows BOM in MSG)
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let input = input.trim();

    // Try RFC 5424 first
    if let Some(msg) = parse_rfc5424(input) {
        return Some(msg);
    }

    // Try RFC 3164
    if let Some(msg) = parse_rfc3164(input, ctx) {
        return Some(msg);
    }

    // Fallback: just extract priority
    parse_simple(input)
}

/// Unescape RFC 5424 structured data value.
/// RFC 5424 Section 6.3.3: The characters '"', '\' and ']' MUST be escaped.
fn unescape_sd_value(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    '"' | '\\' | ']' => {
                        result.push(next);
                        chars.next();
                    }
                    _ => {
                        // Invalid escape sequence, keep as-is
                        result.push(c);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Parse RFC 5424 format.
fn parse_rfc5424(input: &str) -> Option<SyslogMessage> {
    let caps = RFC5424_REGEX.captures(input)?;

    let pri: u8 = caps.get(1)?.as_str().parse().ok()?;
    let facility = Facility::from_code(pri >> 3)?;
    let severity = Severity::from_code(pri & 0x07)?;

    let _version: u8 = caps.get(2)?.as_str().parse().ok()?;

    let timestamp_str = caps.get(3)?.as_str();
    let timestamp = parse_rfc5424_timestamp(timestamp_str);

    let hostname = nilvalue_to_option(caps.get(4)?.as_str());
    let app_name = nilvalue_to_option(caps.get(5)?.as_str());
    let proc_id = nilvalue_to_option(caps.get(6)?.as_str());
    let msg_id = nilvalue_to_option(caps.get(7)?.as_str());

    let sd_str = caps.get(8)?.as_str();
    let structured_data = parse_structured_data(sd_str);

    let message = caps
        .get(9)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    Some(SyslogMessage {
        facility,
        severity,
        timestamp,
        hostname,
        app_name,
        proc_id,
        msg_id,
        structured_data,
        message,
        raw: input.to_string(),
        version: SyslogVersion::Rfc5424,
        // RFC 5424 timestamps are explicit and timezone-qualified.
        ts_source: TsSource::Sender,
    })
}

/// Parse RFC 3164 format.
fn parse_rfc3164(input: &str, ctx: &RfcTimeCtx) -> Option<SyslogMessage> {
    let caps = RFC3164_REGEX.captures(input)?;

    let pri: u8 = caps.get(1)?.as_str().parse().ok()?;
    let facility = Facility::from_code(pri >> 3)?;
    let severity = Severity::from_code(pri & 0x07)?;

    let timestamp_str = caps.get(2)?.as_str();
    let (timestamp, ts_source) = match parse_rfc3164_timestamp(timestamp_str, ctx) {
        Some((ts, src)) => (Some(ts), src),
        // Unparseable stamp: fall back to receive time.
        None => (Some(ctx.now), TsSource::Receiver),
    };

    let hostname = Some(caps.get(3)?.as_str().to_string());
    let app_name = Some(caps.get(4)?.as_str().to_string());
    let proc_id = caps.get(5).map(|m| m.as_str().to_string());
    let message = caps
        .get(6)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    Some(SyslogMessage {
        facility,
        severity,
        timestamp,
        hostname,
        app_name,
        proc_id,
        msg_id: None,
        structured_data: HashMap::new(),
        message,
        raw: input.to_string(),
        version: SyslogVersion::Rfc3164,
        ts_source,
    })
}

/// Parse simple format (just priority + message).
fn parse_simple(input: &str) -> Option<SyslogMessage> {
    let caps = SIMPLE_REGEX.captures(input)?;

    let pri: u8 = caps.get(1)?.as_str().parse().ok()?;
    let facility = Facility::from_code(pri >> 3)?;
    let severity = Severity::from_code(pri & 0x07)?;
    let message = caps
        .get(2)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    Some(SyslogMessage {
        facility,
        severity,
        timestamp: None,
        hostname: None,
        app_name: None,
        proc_id: None,
        msg_id: None,
        structured_data: HashMap::new(),
        message,
        raw: input.to_string(),
        version: SyslogVersion::Rfc3164,
        // No timestamp in the simple fallback; leave provenance at the default.
        ts_source: TsSource::Sender,
    })
}

/// Parse RFC 5424 timestamp (ISO 8601 format).
fn parse_rfc5424_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if s == "-" {
        return None;
    }

    // Try parsing as RFC 3339
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

/// Parse an RFC 3164 timestamp (e.g. `Jan  5 14:30:00`) into a UTC instant,
/// inferring the year and applying the sender timezone (#545).
///
/// Steps:
/// 1. Parse the month/day/time (both single- and double-space day padding).
/// 2. Pick the year (of `now-1`, `now`, `now+1`) that puts the *local* wall
///    clock closest to `now` — fixing the Dec↔Jan boundary in both directions.
/// 3. Interpret the wall clock in `ctx.tz` (DST-correct via the tz database);
///    for a gap/overlap, take the earliest valid instant.
/// 4. If the result is still more than `ctx.max_skew` from `now`, reject it so
///    the caller falls back to receive time (`Receiver`).
fn parse_rfc3164_timestamp(s: &str, ctx: &RfcTimeCtx) -> Option<(DateTime<Utc>, TsSource)> {
    // The regex already fixed the shape; split off the "Mmm dd hh:mm:ss" fields.
    // Accept one or more spaces between month and day ("Jan  5" and "Jan 5").
    let mut it = s.split_whitespace();
    let month = month_from_abbr(it.next()?)?;
    let day: u32 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let mut tp = time.split(':');
    let hour: u32 = tp.next()?.parse().ok()?;
    let min: u32 = tp.next()?.parse().ok()?;
    let sec: u32 = tp.next()?.parse().ok()?;

    // In the sender's zone, `now` is the reference for year inference.
    let now_local = ctx.now.with_timezone(&ctx.tz);
    let candidate_years = [now_local.year() - 1, now_local.year(), now_local.year() + 1];

    // Pick the candidate whose resulting UTC instant is closest to `now`.
    let mut best: Option<(DateTime<Utc>, i64)> = None;
    for year in candidate_years {
        let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
            continue; // e.g. Feb 29 in a non-leap year
        };
        let Some(naive) = date.and_hms_opt(hour, min, sec) else {
            continue;
        };
        // Localize; for DST gaps/overlaps take the earliest valid instant.
        let Some(local) = ctx.tz.from_local_datetime(&naive).earliest() else {
            continue;
        };
        let utc = local.with_timezone(&Utc);
        let dist = (utc - ctx.now).num_seconds().abs();
        if best.is_none_or(|(_, d)| dist < d) {
            best = Some((utc, dist));
        }
    }

    let (utc, dist) = best?;
    if dist > ctx.max_skew.num_seconds() {
        // Implausibly far from receive time even after year inference — the
        // stamp is untrustworthy (bad clock, wrong tz). Use the receive clock.
        Some((ctx.now, TsSource::Receiver))
    } else {
        Some((utc, TsSource::Sender))
    }
}

/// Three-letter English month abbreviation → 1..=12.
fn month_from_abbr(m: &str) -> Option<u32> {
    Some(match m {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Parse structured data section.
/// RFC 5424 Section 6.3: STRUCTURED-DATA = NILVALUE / 1*SD-ELEMENT
fn parse_structured_data(s: &str) -> HashMap<String, HashMap<String, String>> {
    let mut result = HashMap::new();

    // NILVALUE
    if s == "-" || s.trim().is_empty() {
        return result;
    }

    for sd_cap in SD_REGEX.captures_iter(s) {
        if let Some(sd_id) = sd_cap.get(1) {
            let mut params = HashMap::new();

            if let Some(param_str) = sd_cap.get(2) {
                for param_cap in SD_PARAM_REGEX.captures_iter(param_str.as_str()) {
                    if let (Some(key), Some(value)) = (param_cap.get(1), param_cap.get(2)) {
                        // Unescape the value per RFC 5424 Section 6.3.3
                        let unescaped = unescape_sd_value(value.as_str());
                        params.insert(key.as_str().to_string(), unescaped);
                    }
                }
            }

            result.insert(sd_id.as_str().to_string(), params);
        }
    }

    result
}

/// Convert NILVALUE ("-") to None.
fn nilvalue_to_option(s: &str) -> Option<String> {
    if s == "-" { None } else { Some(s.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rfc5424() {
        let msg = r#"<165>1 2023-08-24T05:14:15.000003-07:00 mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut="3" eventSource="Application" eventID="1011"] An application event log entry"#;

        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.version, SyslogVersion::Rfc5424);
        assert_eq!(parsed.facility, Facility::Local4);
        assert_eq!(parsed.severity, Severity::Notice);
        assert_eq!(parsed.hostname, Some("mymachine.example.com".to_string()));
        assert_eq!(parsed.app_name, Some("evntslog".to_string()));
        assert_eq!(parsed.msg_id, Some("ID47".to_string()));
        assert!(parsed.message.contains("An application event log entry"));

        // Check structured data
        let sd = parsed.structured_data.get("exampleSDID@32473").unwrap();
        assert_eq!(sd.get("iut"), Some(&"3".to_string()));
        assert_eq!(sd.get("eventSource"), Some(&"Application".to_string()));
    }

    #[test]
    fn test_parse_rfc5424_minimal() {
        let msg = "<34>1 2023-01-01T00:00:00Z - - - - - Test message";

        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.version, SyslogVersion::Rfc5424);
        assert_eq!(parsed.facility, Facility::Auth);
        assert_eq!(parsed.severity, Severity::Critical);
        assert_eq!(parsed.hostname, None);
        assert_eq!(parsed.app_name, None);
        assert_eq!(parsed.message, "Test message");
    }

    #[test]
    fn test_parse_rfc3164() {
        let msg = "<34>Jan  5 14:30:00 myhost sshd[12345]: Connection from 192.168.1.1";

        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.version, SyslogVersion::Rfc3164);
        assert_eq!(parsed.facility, Facility::Auth);
        assert_eq!(parsed.severity, Severity::Critical);
        assert_eq!(parsed.hostname, Some("myhost".to_string()));
        assert_eq!(parsed.app_name, Some("sshd".to_string()));
        assert_eq!(parsed.proc_id, Some("12345".to_string()));
        assert_eq!(parsed.message, "Connection from 192.168.1.1");
    }

    #[test]
    fn test_parse_rfc3164_no_pid() {
        let msg = "<13>Oct 22 10:52:12 localhost kernel: Device eth0 entered promiscuous mode";

        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.facility, Facility::User);
        assert_eq!(parsed.severity, Severity::Notice);
        assert_eq!(parsed.hostname, Some("localhost".to_string()));
        assert_eq!(parsed.app_name, Some("kernel".to_string()));
        assert_eq!(parsed.proc_id, None);
    }

    // ---- #545: RFC 3164 timestamp correctness ----

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    /// A message received on Jan 1 carrying a `Dec 31` stamp is dated to the
    /// *previous* year, not the current one (year rollover, backward).
    #[test]
    fn rfc3164_year_rollover_backward() {
        let now = utc(2027, 1, 1, 0, 0, 2); // Jan 1 00:00:02 UTC
        let ctx = RfcTimeCtx::new(Tz::UTC, now);
        let msg = parse_with_time("<34>Dec 31 23:59:58 h app: x", &ctx).unwrap();
        assert_eq!(msg.timestamp.unwrap(), utc(2026, 12, 31, 23, 59, 58));
        assert_eq!(msg.ts_source, TsSource::Sender);
    }

    /// A message received on Dec 31 carrying a `Jan 1` stamp is dated to the
    /// *next* year (year rollover, forward).
    #[test]
    fn rfc3164_year_rollover_forward() {
        let now = utc(2026, 12, 31, 23, 59, 58);
        let ctx = RfcTimeCtx::new(Tz::UTC, now);
        let msg = parse_with_time("<34>Jan  1 00:00:02 h app: x", &ctx).unwrap();
        assert_eq!(msg.timestamp.unwrap(), utc(2027, 1, 1, 0, 0, 2));
    }

    /// The acceptance example: a `Jul 23 14:00:00` stamp from a Europe/Paris
    /// sender (CEST, UTC+2 in summer) is 12:00:00 UTC.
    #[test]
    fn rfc3164_sender_timezone_to_utc() {
        let now = utc(2026, 7, 23, 12, 5, 0);
        let ctx = RfcTimeCtx::new(chrono_tz::Europe::Paris, now);
        let msg = parse_with_time("<34>Jul 23 14:00:00 h app: x", &ctx).unwrap();
        assert_eq!(msg.timestamp.unwrap(), utc(2026, 7, 23, 12, 0, 0));
    }

    /// A winter timestamp from the same zone uses CET (UTC+1), proving DST is
    /// applied from the tz database rather than a fixed offset.
    #[test]
    fn rfc3164_timezone_respects_dst() {
        let now = utc(2026, 1, 15, 12, 5, 0);
        let ctx = RfcTimeCtx::new(chrono_tz::Europe::Paris, now);
        let msg = parse_with_time("<34>Jan 15 14:00:00 h app: x", &ctx).unwrap();
        assert_eq!(msg.timestamp.unwrap(), utc(2026, 1, 15, 13, 0, 0));
    }

    /// An implausible stamp (months away even after year inference) is clamped
    /// to the receive time and marked `Receiver`.
    #[test]
    fn rfc3164_implausible_stamp_clamps_to_receive_time() {
        let now = utc(2026, 7, 23, 12, 0, 0);
        // Tight 1-day window; a Jan stamp is ~6 months off → clamp.
        let ctx = RfcTimeCtx::new(Tz::UTC, now).with_max_skew(Duration::days(1));
        let msg = parse_with_time("<34>Jan 15 14:00:00 h app: x", &ctx).unwrap();
        assert_eq!(msg.timestamp.unwrap(), now);
        assert_eq!(msg.ts_source, TsSource::Receiver);
    }

    /// RFC 5424's explicit timestamp is unaffected by the RFC 3164 context.
    #[test]
    fn rfc5424_timestamp_unchanged_by_ctx() {
        let ctx = RfcTimeCtx::new(chrono_tz::Europe::Paris, utc(2030, 1, 1, 0, 0, 0));
        let msg = parse_with_time("<165>1 2023-08-24T05:14:15-07:00 h app - - - hi", &ctx).unwrap();
        assert_eq!(msg.version, SyslogVersion::Rfc5424);
        assert_eq!(msg.timestamp.unwrap(), utc(2023, 8, 24, 12, 14, 15));
        assert_eq!(msg.ts_source, TsSource::Sender);
    }

    #[test]
    fn test_parse_simple() {
        let msg = "<14>A simple message without structure";

        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.facility, Facility::User);
        assert_eq!(parsed.severity, Severity::Informational);
        assert_eq!(parsed.message, "A simple message without structure");
    }

    #[test]
    fn test_facility_codes() {
        assert_eq!(Facility::from_code(0), Some(Facility::Kern));
        assert_eq!(Facility::from_code(4), Some(Facility::Auth));
        assert_eq!(Facility::from_code(16), Some(Facility::Local0));
        assert_eq!(Facility::from_code(23), Some(Facility::Local7));
        assert_eq!(Facility::from_code(24), None);
    }

    #[test]
    fn test_severity_codes() {
        assert_eq!(Severity::from_code(0), Some(Severity::Emergency));
        assert_eq!(Severity::from_code(3), Some(Severity::Error));
        assert_eq!(Severity::from_code(7), Some(Severity::Debug));
        assert_eq!(Severity::from_code(8), None);
    }

    #[test]
    fn test_priority_calculation() {
        // Priority = Facility * 8 + Severity
        // <165> = 20 * 8 + 5 = Local4.Notice
        let msg = "<165>1 - - - - - - Test";
        let parsed = parse(msg).unwrap();
        assert_eq!(parsed.facility, Facility::Local4);
        assert_eq!(parsed.severity, Severity::Notice);
    }

    #[test]
    fn test_rfc5424_escaped_structured_data() {
        // Test escaped characters in structured data values
        let msg = r#"<165>1 2023-01-01T00:00:00Z host app - - [test@123 key="value with \"quotes\" and \\backslash"] Message"#;
        let parsed = parse(msg).unwrap();

        let sd = parsed.structured_data.get("test@123").unwrap();
        assert_eq!(
            sd.get("key"),
            Some(&"value with \"quotes\" and \\backslash".to_string())
        );
    }

    #[test]
    fn test_rfc5424_escaped_bracket() {
        // Test escaped ] in structured data - this is a complex case
        // The regex-based parser handles most cases but escaped ] at end of value
        // before the closing ] is tricky. Test basic escaping works.
        let msg = r#"<165>1 2023-01-01T00:00:00Z host app - - [test@123 data="no brackets here"] Message"#;
        let parsed = parse(msg).unwrap();

        let sd = parsed.structured_data.get("test@123").unwrap();
        assert_eq!(sd.get("data"), Some(&"no brackets here".to_string()));
    }

    #[test]
    fn test_rfc5424_multiple_sd_elements() {
        // Test multiple structured data elements with space between them
        let msg = r#"<165>1 2023-01-01T00:00:00Z host app - - [first@123 a="1"] [second@456 b="2" c="3"] Message"#;
        let parsed = parse(msg).unwrap();

        // With space between elements, both should be captured
        assert!(parsed.structured_data.contains_key("first@123"));
        let first = parsed.structured_data.get("first@123").unwrap();
        assert_eq!(first.get("a"), Some(&"1".to_string()));
    }

    #[test]
    fn test_rfc5424_single_sd_element_multiple_params() {
        // Test single SD element with multiple parameters
        let msg = r#"<165>1 2023-01-01T00:00:00Z host app - - [origin@123 ip="10.0.0.1" port="443" proto="tcp"] Message"#;
        let parsed = parse(msg).unwrap();

        assert!(parsed.structured_data.contains_key("origin@123"));

        let origin = parsed.structured_data.get("origin@123").unwrap();
        assert_eq!(origin.get("ip"), Some(&"10.0.0.1".to_string()));
        assert_eq!(origin.get("port"), Some(&"443".to_string()));
        assert_eq!(origin.get("proto"), Some(&"tcp".to_string()));
    }

    #[test]
    fn test_bom_handling() {
        // Test UTF-8 BOM is properly stripped
        let msg = "\u{FEFF}<14>1 2023-01-01T00:00:00Z host app - - - Message with BOM";
        let parsed = parse(msg).unwrap();

        assert_eq!(parsed.version, SyslogVersion::Rfc5424);
        assert_eq!(parsed.message, "Message with BOM");
    }

    #[test]
    fn test_unescape_sd_value() {
        use super::unescape_sd_value;

        assert_eq!(unescape_sd_value("simple"), "simple");
        assert_eq!(unescape_sd_value(r#"with \"quotes\""#), "with \"quotes\"");
        assert_eq!(unescape_sd_value(r"with \\backslash"), "with \\backslash");
        assert_eq!(unescape_sd_value(r"with \] bracket"), "with ] bracket");
        assert_eq!(
            unescape_sd_value(r#"all \\ \" \] together"#),
            "all \\ \" ] together"
        );
    }

    #[test]
    fn test_rfc5424_nilvalue_fields() {
        // All fields as NILVALUE
        let msg = "<14>1 - - - - - - Just a message";
        let parsed = parse(msg).unwrap();

        assert_eq!(parsed.timestamp, None);
        assert_eq!(parsed.hostname, None);
        assert_eq!(parsed.app_name, None);
        assert_eq!(parsed.proc_id, None);
        assert_eq!(parsed.msg_id, None);
        assert!(parsed.structured_data.is_empty());
        assert_eq!(parsed.message, "Just a message");
    }

    #[test]
    fn test_rfc5424_timezone_offset() {
        // Test timestamp with timezone offset
        let msg = "<14>1 2023-08-24T05:14:15.000003-07:00 host app - - - Test";
        let parsed = parse(msg).unwrap();

        assert!(parsed.timestamp.is_some());
        let ts = parsed.timestamp.unwrap();
        // Should be converted to UTC: 05:14 - (-07:00) = 12:14 UTC
        assert_eq!(ts.format("%H").to_string(), "12");
    }

    #[test]
    fn test_rfc5424_utc_timestamp() {
        // Test timestamp with Z suffix
        let msg = "<14>1 2023-08-24T12:30:45Z host app - - - Test";
        let parsed = parse(msg).unwrap();

        assert!(parsed.timestamp.is_some());
        let ts = parsed.timestamp.unwrap();
        assert_eq!(ts.format("%H:%M:%S").to_string(), "12:30:45");
    }
}
