//! Mapping from ZenSight syslog TelemetryPoints to OpenTelemetry logs.

use opentelemetry::logs::Severity;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Syslog severity — the one canonical model (#557), re-exported. The
/// exporter-specific bits (combined number/name parse, OTel-crate mapping, and
/// the full-name `severity_text` this exporter emits) are free helpers below.
pub use zensight_common::LogSeverity as SyslogSeverity;

/// Parse a severity from a number or a name (`"3"` / `"err"` / `"error"`).
pub fn parse_severity(s: &str) -> Option<SyslogSeverity> {
    if let Ok(n) = s.parse::<u8>() {
        return SyslogSeverity::from_code(n);
    }
    SyslogSeverity::from_slug(s)
}

/// Map to the OpenTelemetry `Severity` enum.
pub fn to_otel_severity(sev: SyslogSeverity) -> Severity {
    match sev {
        SyslogSeverity::Emergency | SyslogSeverity::Alert => Severity::Fatal,
        SyslogSeverity::Critical | SyslogSeverity::Error => Severity::Error,
        SyslogSeverity::Warning => Severity::Warn,
        SyslogSeverity::Notice | SyslogSeverity::Informational => Severity::Info,
        SyslogSeverity::Debug => Severity::Debug,
    }
}

/// The full-name severity text this exporter emits for OTel `severity_text` +
/// the `syslog.severity` attribute (preserved from the pre-#557 mapping — note
/// `informational` renders as `info`, matching the prior behavior).
pub fn severity_text(sev: SyslogSeverity) -> &'static str {
    match sev {
        SyslogSeverity::Emergency => "emergency",
        SyslogSeverity::Alert => "alert",
        SyslogSeverity::Critical => "critical",
        SyslogSeverity::Error => "error",
        SyslogSeverity::Warning => "warning",
        SyslogSeverity::Notice => "notice",
        SyslogSeverity::Informational => "info",
        SyslogSeverity::Debug => "debug",
    }
}

/// Syslog facilities (RFC 5424).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogFacility {
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
    Security = 13,
    Console = 14,
    SolarisCron = 15,
    Local0 = 16,
    Local1 = 17,
    Local2 = 18,
    Local3 = 19,
    Local4 = 20,
    Local5 = 21,
    Local6 = 22,
    Local7 = 23,
}

impl SyslogFacility {
    /// Create from numeric facility.
    pub fn from_number(n: u8) -> Option<Self> {
        match n {
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
            13 => Some(Self::Security),
            14 => Some(Self::Console),
            15 => Some(Self::SolarisCron),
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

    /// Parse from a string (number or name).
    pub fn parse(s: &str) -> Option<Self> {
        if let Ok(n) = s.parse::<u8>() {
            return Self::from_number(n);
        }

        match s.to_lowercase().as_str() {
            "kern" => Some(Self::Kern),
            "user" => Some(Self::User),
            "mail" => Some(Self::Mail),
            "daemon" => Some(Self::Daemon),
            "auth" => Some(Self::Auth),
            "syslog" => Some(Self::Syslog),
            "lpr" => Some(Self::Lpr),
            "news" => Some(Self::News),
            "uucp" => Some(Self::Uucp),
            "cron" => Some(Self::Cron),
            "authpriv" => Some(Self::Authpriv),
            "ftp" => Some(Self::Ftp),
            "ntp" => Some(Self::Ntp),
            "security" => Some(Self::Security),
            "console" => Some(Self::Console),
            "local0" => Some(Self::Local0),
            "local1" => Some(Self::Local1),
            "local2" => Some(Self::Local2),
            "local3" => Some(Self::Local3),
            "local4" => Some(Self::Local4),
            "local5" => Some(Self::Local5),
            "local6" => Some(Self::Local6),
            "local7" => Some(Self::Local7),
            _ => None,
        }
    }

    /// Get the facility name.
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
            Self::Security => "security",
            Self::Console => "console",
            Self::SolarisCron => "solaris-cron",
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

/// Extracted log data from a syslog TelemetryPoint.
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// Log message body.
    pub body: String,
    /// Severity level.
    pub severity: SyslogSeverity,
    /// Facility.
    pub facility: Option<SyslogFacility>,
    /// Application name.
    pub appname: Option<String>,
    /// Hostname/source.
    pub hostname: String,
    /// Timestamp in nanoseconds since epoch.
    pub timestamp_nanos: i64,
    /// OTel `log.record.uid` — stable per-line id (#104), if the sensor set it.
    pub uid: Option<String>,
    /// OTel `log.record.original` — the verbatim raw line (#104), if present.
    pub original: Option<String>,
}

impl LogRecord {
    /// Try to extract a log record from a TelemetryPoint.
    ///
    /// Returns None if the point is not a syslog text message.
    pub fn from_telemetry(point: &TelemetryPoint) -> Option<Self> {
        // Only process syslog text messages
        if point.protocol != Protocol::Logs {
            return None;
        }

        let body = match &point.value {
            TelemetryValue::Text(s) => s.clone(),
            _ => return None,
        };

        // Extract severity from labels
        let severity = point
            .labels
            .get("severity")
            .and_then(|s| parse_severity(s))
            .unwrap_or(SyslogSeverity::Informational);

        // Extract facility from labels
        let facility = point
            .labels
            .get("facility")
            .and_then(|s| SyslogFacility::parse(s));

        // Extract appname from labels
        let appname = point.labels.get("appname").cloned();

        Some(Self {
            body,
            severity,
            facility,
            appname,
            hostname: point.source.clone(),
            timestamp_nanos: point.timestamp * 1_000_000, // ms to ns
            uid: point.labels.get("log.record.uid").cloned(),
            original: point.labels.get("log.record.original").cloned(),
        })
    }

    /// Get the OpenTelemetry severity.
    pub fn otel_severity(&self) -> Severity {
        to_otel_severity(self.severity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_syslog_severity_from_str() {
        assert_eq!(parse_severity("0"), Some(SyslogSeverity::Emergency));
        assert_eq!(parse_severity("3"), Some(SyslogSeverity::Error));
        assert_eq!(parse_severity("7"), Some(SyslogSeverity::Debug));

        assert_eq!(parse_severity("emergency"), Some(SyslogSeverity::Emergency));
        assert_eq!(parse_severity("error"), Some(SyslogSeverity::Error));
        assert_eq!(parse_severity("warning"), Some(SyslogSeverity::Warning));
        assert_eq!(parse_severity("info"), Some(SyslogSeverity::Informational));

        assert_eq!(parse_severity("invalid"), None);
    }

    #[test]
    fn test_syslog_severity_to_otel() {
        assert_eq!(to_otel_severity(SyslogSeverity::Emergency), Severity::Fatal);
        assert_eq!(to_otel_severity(SyslogSeverity::Error), Severity::Error);
        assert_eq!(to_otel_severity(SyslogSeverity::Warning), Severity::Warn);
        assert_eq!(to_otel_severity(SyslogSeverity::Notice), Severity::Info);
        assert_eq!(to_otel_severity(SyslogSeverity::Debug), Severity::Debug);
    }

    #[test]
    fn test_syslog_facility_from_str() {
        assert_eq!(SyslogFacility::parse("0"), Some(SyslogFacility::Kern));
        assert_eq!(SyslogFacility::parse("3"), Some(SyslogFacility::Daemon));
        assert_eq!(
            SyslogFacility::parse("daemon"),
            Some(SyslogFacility::Daemon)
        );
        assert_eq!(
            SyslogFacility::parse("local0"),
            Some(SyslogFacility::Local0)
        );

        assert_eq!(SyslogFacility::parse("invalid"), None);
    }

    #[test]
    fn test_log_record_from_telemetry() {
        let mut labels = HashMap::new();
        labels.insert("severity".to_string(), "warning".to_string());
        labels.insert("facility".to_string(), "daemon".to_string());
        labels.insert("appname".to_string(), "nginx".to_string());

        let point = TelemetryPoint {
            timestamp: 1234567890000,
            source: "server01".to_string(),
            protocol: Protocol::Logs,
            metric: "message".to_string(),
            value: TelemetryValue::Text("Connection refused".to_string()),
            labels,
            unit: None,
        };

        let record = LogRecord::from_telemetry(&point).unwrap();

        assert_eq!(record.body, "Connection refused");
        assert_eq!(record.severity, SyslogSeverity::Warning);
        assert_eq!(record.facility, Some(SyslogFacility::Daemon));
        assert_eq!(record.appname, Some("nginx".to_string()));
        assert_eq!(record.hostname, "server01");
        assert_eq!(record.timestamp_nanos, 1_234_567_890_000_000_000);
    }

    #[test]
    fn test_log_record_non_syslog() {
        let point = TelemetryPoint {
            timestamp: 1234567890000,
            source: "router01".to_string(),
            protocol: Protocol::Snmp,
            metric: "sysDescr".to_string(),
            value: TelemetryValue::Text("Cisco Router".to_string()),
            labels: HashMap::new(),
            unit: None,
        };

        assert!(LogRecord::from_telemetry(&point).is_none());
    }

    #[test]
    fn test_log_record_non_text() {
        let point = TelemetryPoint {
            timestamp: 1234567890000,
            source: "server01".to_string(),
            protocol: Protocol::Logs,
            metric: "count".to_string(),
            value: TelemetryValue::Counter(100),
            labels: HashMap::new(),
            unit: None,
        };

        assert!(LogRecord::from_telemetry(&point).is_none());
    }
}
