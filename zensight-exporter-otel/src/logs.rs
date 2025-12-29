//! Mapping from ZenSight syslog TelemetryPoints to OpenTelemetry logs.

use opentelemetry::logs::Severity;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Syslog severity levels (RFC 5424).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyslogSeverity {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Informational = 6,
    Debug = 7,
}

impl SyslogSeverity {
    /// Parse from a string (number or name).
    pub fn parse(s: &str) -> Option<Self> {
        // Try numeric first
        if let Ok(n) = s.parse::<u8>() {
            return Self::from_number(n);
        }

        // Try name
        match s.to_lowercase().as_str() {
            "emergency" | "emerg" => Some(Self::Emergency),
            "alert" => Some(Self::Alert),
            "critical" | "crit" => Some(Self::Critical),
            "error" | "err" => Some(Self::Error),
            "warning" | "warn" => Some(Self::Warning),
            "notice" => Some(Self::Notice),
            "informational" | "info" => Some(Self::Informational),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    /// Create from numeric severity.
    pub fn from_number(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::Emergency),
            1 => Some(Self::Alert),
            2 => Some(Self::Critical),
            3 => Some(Self::Error),
            4 => Some(Self::Warning),
            5 => Some(Self::Notice),
            6 => Some(Self::Informational),
            7 => Some(Self::Debug),
            _ => None,
        }
    }

    /// Convert to OpenTelemetry Severity.
    pub fn to_otel_severity(self) -> Severity {
        match self {
            Self::Emergency => Severity::Fatal,
            Self::Alert => Severity::Fatal,
            Self::Critical => Severity::Error,
            Self::Error => Severity::Error,
            Self::Warning => Severity::Warn,
            Self::Notice => Severity::Info,
            Self::Informational => Severity::Info,
            Self::Debug => Severity::Debug,
        }
    }

    /// Get the severity text name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emergency => "emergency",
            Self::Alert => "alert",
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Notice => "notice",
            Self::Informational => "info",
            Self::Debug => "debug",
        }
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
}

impl LogRecord {
    /// Try to extract a log record from a TelemetryPoint.
    ///
    /// Returns None if the point is not a syslog text message.
    pub fn from_telemetry(point: &TelemetryPoint) -> Option<Self> {
        // Only process syslog text messages
        if point.protocol != Protocol::Syslog {
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
            .and_then(|s| SyslogSeverity::parse(s))
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
        })
    }

    /// Get the OpenTelemetry severity.
    pub fn otel_severity(&self) -> Severity {
        self.severity.to_otel_severity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_syslog_severity_from_str() {
        assert_eq!(SyslogSeverity::parse("0"), Some(SyslogSeverity::Emergency));
        assert_eq!(SyslogSeverity::parse("3"), Some(SyslogSeverity::Error));
        assert_eq!(SyslogSeverity::parse("7"), Some(SyslogSeverity::Debug));

        assert_eq!(
            SyslogSeverity::parse("emergency"),
            Some(SyslogSeverity::Emergency)
        );
        assert_eq!(SyslogSeverity::parse("error"), Some(SyslogSeverity::Error));
        assert_eq!(
            SyslogSeverity::parse("warning"),
            Some(SyslogSeverity::Warning)
        );
        assert_eq!(
            SyslogSeverity::parse("info"),
            Some(SyslogSeverity::Informational)
        );

        assert_eq!(SyslogSeverity::parse("invalid"), None);
    }

    #[test]
    fn test_syslog_severity_to_otel() {
        assert_eq!(
            SyslogSeverity::Emergency.to_otel_severity(),
            Severity::Fatal
        );
        assert_eq!(SyslogSeverity::Error.to_otel_severity(), Severity::Error);
        assert_eq!(SyslogSeverity::Warning.to_otel_severity(), Severity::Warn);
        assert_eq!(SyslogSeverity::Notice.to_otel_severity(), Severity::Info);
        assert_eq!(SyslogSeverity::Debug.to_otel_severity(), Severity::Debug);
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
            protocol: Protocol::Syslog,
            metric: "message".to_string(),
            value: TelemetryValue::Text("Connection refused".to_string()),
            labels,
        };

        let record = LogRecord::from_telemetry(&point).unwrap();

        assert_eq!(record.body, "Connection refused");
        assert_eq!(record.severity, SyslogSeverity::Warning);
        assert_eq!(record.facility, Some(SyslogFacility::Daemon));
        assert_eq!(record.appname, Some("nginx".to_string()));
        assert_eq!(record.hostname, "server01");
        assert_eq!(record.timestamp_nanos, 1234567890000_000_000);
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
        };

        assert!(LogRecord::from_telemetry(&point).is_none());
    }

    #[test]
    fn test_log_record_non_text() {
        let point = TelemetryPoint {
            timestamp: 1234567890000,
            source: "server01".to_string(),
            protocol: Protocol::Syslog,
            metric: "count".to_string(),
            value: TelemetryValue::Counter(100),
            labels: HashMap::new(),
        };

        assert!(LogRecord::from_telemetry(&point).is_none());
    }
}
