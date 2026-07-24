//! Server-side log search (#553): compile the content selectors on the events
//! query into a [`LogMatcher`] applied at the sensor over the ring + durable
//! store, so "find every line matching X in the last N hours" is answered
//! without shipping the whole store to the client.
//!
//! Regex safety: the `regex` crate is linear-time (RE2-style — no catastrophic
//! backtracking), and we additionally cap the compiled program size, so a
//! pathological pattern is rejected at compile rather than pinning a core. A
//! metacharacter-free pattern skips the engine entirely (substring fast path).
//! Unbounded *scan* cost over a long range is bounded by a per-query scan cap in
//! the store walk, not here.

use regex::RegexBuilder;
use zensight_common::LogRecord;

/// Compiled program-size ceiling for a search regex (bytes). Generous for real
/// patterns, tight enough to reject a compile-bomb.
const REGEX_SIZE_LIMIT: usize = 1 << 20; // 1 MiB

/// The message-content test: a linear-time regex, or a plain substring when the
/// pattern has no regex metacharacters (measurably cheaper).
enum PatternMatch {
    Substring(String),
    Regex(Box<regex::Regex>),
}

impl PatternMatch {
    fn is_match(&self, msg: &str) -> bool {
        match self {
            PatternMatch::Substring(s) => msg.contains(s.as_str()),
            PatternMatch::Regex(re) => re.is_match(msg),
        }
    }
}

/// A compiled set of content selectors. An empty matcher matches every record;
/// [`LogMatcher::is_trivial`] reports that so callers can skip it.
pub struct LogMatcher {
    pattern: Option<PatternMatch>,
    /// Syslog severity number (0=emerg … 7=debug); match records at least this
    /// severe (`<=`).
    severity_max_num: Option<u8>,
    unit: Option<String>,
    app: Option<String>,
    facility: Option<String>,
}

impl LogMatcher {
    /// Build a matcher from the query selector values. `pattern` compiles a regex
    /// (or takes the substring fast path); returns an error string if the regex
    /// is invalid or too large. All args are optional.
    pub fn new(
        pattern: Option<&str>,
        severity_min: Option<&str>,
        unit: Option<&str>,
        app: Option<&str>,
        facility: Option<&str>,
    ) -> Result<Self, String> {
        let pattern = match pattern.filter(|p| !p.is_empty()) {
            Some(p) if has_regex_meta(p) => {
                let re = RegexBuilder::new(p)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .dfa_size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| format!("invalid or too-large pattern: {e}"))?;
                Some(PatternMatch::Regex(Box::new(re)))
            }
            Some(p) => Some(PatternMatch::Substring(p.to_string())),
            None => None,
        };
        Ok(Self {
            pattern,
            severity_max_num: severity_min.and_then(parse_severity_min),
            unit: unit.filter(|s| !s.is_empty()).map(str::to_string),
            app: app.filter(|s| !s.is_empty()).map(str::to_string),
            facility: facility.filter(|s| !s.is_empty()).map(str::to_string),
        })
    }

    /// True when no selector is set — every record matches, so the caller can
    /// skip per-record evaluation entirely.
    pub fn is_trivial(&self) -> bool {
        self.pattern.is_none()
            && self.severity_max_num.is_none()
            && self.unit.is_none()
            && self.app.is_none()
            && self.facility.is_none()
    }

    /// Test a record. Cheap prefilters (severity/unit/app/facility from stored
    /// fields) run before the regex, so most non-matches never touch the engine.
    pub fn matches(&self, r: &LogRecord) -> bool {
        if let Some(max) = self.severity_max_num
            && severity_slug_num(&r.severity).is_none_or(|n| n > max)
        {
            return false;
        }
        if let Some(f) = &self.facility
            && &r.facility != f
        {
            return false;
        }
        if let Some(a) = &self.app
            && r.app.as_deref() != Some(a.as_str())
        {
            return false;
        }
        if let Some(u) = &self.unit
            && record_unit(r) != Some(u.as_str())
        {
            return false;
        }
        if let Some(p) = &self.pattern
            && !p.is_match(&r.message)
        {
            return false;
        }
        true
    }
}

/// The unit a record is attributed to — journald `_SYSTEMD_UNIT` flows as the
/// `sd.journald.unit` label (file sources set it too).
fn record_unit(r: &LogRecord) -> Option<&str> {
    r.labels.get("sd.journald.unit").map(String::as_str)
}

/// Does `p` contain any regex metacharacter? If not, a substring test is exact
/// and much cheaper than the engine.
fn has_regex_meta(p: &str) -> bool {
    p.chars().any(|c| {
        matches!(
            c,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'
        )
    })
}

/// Parse a `severity_min` selector: a syslog slug (`warning`) or number (`4`),
/// yielding the syslog severity number to compare with `<=`.
fn parse_severity_min(v: &str) -> Option<u8> {
    let v = v.trim();
    if let Ok(n) = v.parse::<u8>() {
        return (n <= 7).then_some(n);
    }
    severity_slug_num(v)
}

/// Syslog severity slug → number (0=emerg … 7=debug). Accepts the parser's
/// slugs plus common aliases.
fn severity_slug_num(slug: &str) -> Option<u8> {
    match slug.trim().to_ascii_lowercase().as_str() {
        "emerg" | "emergency" => Some(0),
        "alert" => Some(1),
        "crit" | "critical" => Some(2),
        "err" | "error" => Some(3),
        "warning" | "warn" => Some(4),
        "notice" => Some(5),
        "info" | "informational" => Some(6),
        "debug" => Some(7),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        sev: &str,
        facility: &str,
        app: Option<&str>,
        unit: Option<&str>,
        msg: &str,
    ) -> LogRecord {
        let mut labels = std::collections::HashMap::new();
        if let Some(u) = unit {
            labels.insert("sd.journald.unit".to_string(), u.to_string());
        }
        LogRecord {
            uid: "u".into(),
            ts: 0,
            host: "h".into(),
            facility: facility.into(),
            severity: sev.into(),
            severity_number: 9,
            app: app.map(str::to_string),
            pid: None,
            message: msg.into(),
            labels,
        }
    }

    #[test]
    fn trivial_matcher_matches_all() {
        let m = LogMatcher::new(None, None, None, None, None).unwrap();
        assert!(m.is_trivial());
        assert!(m.matches(&rec("info", "user", None, None, "anything")));
    }

    #[test]
    fn substring_fast_path_and_regex() {
        // No metacharacters → substring.
        let m = LogMatcher::new(Some("I/O error"), None, None, None, None).unwrap();
        assert!(matches!(m.pattern, Some(PatternMatch::Substring(_))));
        assert!(m.matches(&rec("err", "kern", None, None, "block I/O error on sda")));
        assert!(!m.matches(&rec("err", "kern", None, None, "all good")));

        // Metacharacters → regex, case-insensitive.
        let m = LogMatcher::new(Some("(?i)i/o error"), None, None, None, None).unwrap();
        assert!(matches!(m.pattern, Some(PatternMatch::Regex(_))));
        assert!(m.matches(&rec("err", "kern", None, None, "Block I/O Error")));
    }

    #[test]
    fn severity_min_is_worse_or_equal() {
        let m = LogMatcher::new(None, Some("warning"), None, None, None).unwrap();
        assert!(m.matches(&rec("err", "user", None, None, "x"))); // err(3) <= 4
        assert!(m.matches(&rec("warning", "user", None, None, "x"))); // 4 <= 4
        assert!(!m.matches(&rec("info", "user", None, None, "x"))); // info(6) > 4
        // Numeric form works too.
        let m = LogMatcher::new(None, Some("3"), None, None, None).unwrap();
        assert!(!m.matches(&rec("warning", "user", None, None, "x"))); // 4 > 3
    }

    #[test]
    fn unit_app_facility_prefilters() {
        let m = LogMatcher::new(None, None, Some("nginx.service"), None, None).unwrap();
        assert!(m.matches(&rec("info", "daemon", None, Some("nginx.service"), "x")));
        assert!(!m.matches(&rec("info", "daemon", None, Some("other.service"), "x")));

        let m = LogMatcher::new(None, None, None, Some("sshd"), Some("auth")).unwrap();
        assert!(m.matches(&rec("info", "auth", Some("sshd"), None, "x")));
        assert!(!m.matches(&rec("info", "auth", Some("cron"), None, "x")));
        assert!(!m.matches(&rec("info", "daemon", Some("sshd"), None, "x")));
    }

    #[test]
    fn combined_selectors_and() {
        let m = LogMatcher::new(Some("timeout"), Some("warning"), None, None, None).unwrap();
        assert!(m.matches(&rec("err", "user", None, None, "connection timeout")));
        assert!(!m.matches(&rec("info", "user", None, None, "connection timeout"))); // sev fails
        assert!(!m.matches(&rec("err", "user", None, None, "connection ok"))); // pattern fails
    }

    #[test]
    fn oversized_regex_is_rejected() {
        // A compile-bomb: huge bounded repetition blows the size limit.
        let pat = format!("(?:a{{1000}}){{1000}}{}", "(x)".repeat(50));
        assert!(LogMatcher::new(Some(&pat), None, None, None, None).is_err());
    }
}
