use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single telemetry data point emitted by sensors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPoint {
    /// Unix epoch milliseconds when the measurement was taken.
    pub timestamp: i64,

    /// Device/host identifier (e.g., "router01", "192.168.1.1").
    pub source: String,

    /// Origin protocol.
    pub protocol: Protocol,

    /// Metric name/path (e.g., "system/sysUpTime", "if/1/ifInOctets").
    pub metric: String,

    /// The measured value.
    pub value: TelemetryValue,

    /// Additional context labels (e.g., OID, interface name).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl TelemetryPoint {
    /// Create a new telemetry point with the current timestamp.
    pub fn new(
        source: impl Into<String>,
        protocol: Protocol,
        metric: impl Into<String>,
        value: TelemetryValue,
    ) -> Self {
        Self {
            timestamp: current_timestamp_millis(),
            source: source.into(),
            protocol,
            metric: metric.into(),
            value,
            labels: HashMap::new(),
        }
    }

    /// Add a label to this telemetry point.
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Add multiple labels to this telemetry point.
    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels.extend(labels);
        self
    }
}

/// Typed telemetry value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value")]
pub enum TelemetryValue {
    /// Counter (monotonically increasing).
    #[serde(rename = "counter")]
    Counter(u64),

    /// Gauge (can go up or down).
    #[serde(rename = "gauge")]
    Gauge(f64),

    /// Text value.
    #[serde(rename = "text")]
    Text(String),

    /// Boolean value.
    #[serde(rename = "boolean")]
    Boolean(bool),

    /// Binary data.
    #[serde(rename = "binary")]
    Binary(Vec<u8>),
}

impl From<u64> for TelemetryValue {
    fn from(v: u64) -> Self {
        TelemetryValue::Counter(v)
    }
}

impl From<i64> for TelemetryValue {
    fn from(v: i64) -> Self {
        if v >= 0 {
            TelemetryValue::Counter(v as u64)
        } else {
            TelemetryValue::Gauge(v as f64)
        }
    }
}

impl From<f64> for TelemetryValue {
    fn from(v: f64) -> Self {
        TelemetryValue::Gauge(v)
    }
}

impl From<String> for TelemetryValue {
    fn from(v: String) -> Self {
        TelemetryValue::Text(v)
    }
}

impl From<&str> for TelemetryValue {
    fn from(v: &str) -> Self {
        TelemetryValue::Text(v.to_string())
    }
}

impl From<bool> for TelemetryValue {
    fn from(v: bool) -> Self {
        TelemetryValue::Boolean(v)
    }
}

impl From<Vec<u8>> for TelemetryValue {
    fn from(v: Vec<u8>) -> Self {
        TelemetryValue::Binary(v)
    }
}

/// Protocol identifier for telemetry sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Snmp,
    /// Unified logs (syslog RFC 3164/5424 + systemd-journald). Wire token `logs`
    /// (keyspace v2 / #104; was `syslog`).
    Logs,
    Gnmi,
    Netflow,
    Opcua,
    Modbus,
    Sysinfo,
    /// Linux kernel networking state (interfaces, sockets, routes) via netlink.
    Netlink,
    /// Wire-level packet/flow telemetry via netring (AF_PACKET / AF_XDP).
    Netring,
    /// systemd unit/service state + resource accounting via the D-Bus Manager API.
    Systemd,
}

impl Protocol {
    /// Get the string representation used in key expressions.
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Snmp => "snmp",
            Protocol::Logs => "logs",
            Protocol::Gnmi => "gnmi",
            Protocol::Netflow => "netflow",
            Protocol::Opcua => "opcua",
            Protocol::Modbus => "modbus",
            Protocol::Sysinfo => "sysinfo",
            Protocol::Netlink => "netlink",
            Protocol::Netring => "netring",
            Protocol::Systemd => "systemd",
        }
    }

    /// Human-facing display name for the GUI. Distinct from [`as_str`](Self::as_str)
    /// (the wire/keyspace token `logs`): title-cased for the UI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Protocol::Logs => "Logs",
            _ => self.as_str(),
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "snmp" => Ok(Protocol::Snmp),
            "logs" => Ok(Protocol::Logs),
            "gnmi" => Ok(Protocol::Gnmi),
            "netflow" => Ok(Protocol::Netflow),
            "opcua" => Ok(Protocol::Opcua),
            "modbus" => Ok(Protocol::Modbus),
            "sysinfo" => Ok(Protocol::Sysinfo),
            "netlink" => Ok(Protocol::Netlink),
            "netring" => Ok(Protocol::Netring),
            "systemd" => Ok(Protocol::Systemd),
            _ => Err(()),
        }
    }
}

/// Get the current timestamp in milliseconds since Unix epoch.
///
/// Returns 0 if system time is before Unix epoch (should never happen in practice).
pub fn current_timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_point_creation() {
        let point = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        )
        .with_label("oid", "1.3.6.1.2.1.1.3.0");

        assert_eq!(point.source, "router01");
        assert_eq!(point.protocol, Protocol::Snmp);
        assert_eq!(point.metric, "system/sysUpTime");
        assert_eq!(point.value, TelemetryValue::Counter(123456));
        assert_eq!(
            point.labels.get("oid"),
            Some(&"1.3.6.1.2.1.1.3.0".to_string())
        );
    }

    #[test]
    fn test_protocol_display() {
        assert_eq!(Protocol::Snmp.as_str(), "snmp");
        assert_eq!(Protocol::Logs.as_str(), "logs");
    }

    #[test]
    fn test_protocol_from_str() {
        assert_eq!("snmp".parse::<Protocol>(), Ok(Protocol::Snmp));
        assert_eq!("logs".parse::<Protocol>(), Ok(Protocol::Logs));
        assert_eq!("gnmi".parse::<Protocol>(), Ok(Protocol::Gnmi));
        assert_eq!("netflow".parse::<Protocol>(), Ok(Protocol::Netflow));
        assert_eq!("opcua".parse::<Protocol>(), Ok(Protocol::Opcua));
        assert_eq!("modbus".parse::<Protocol>(), Ok(Protocol::Modbus));
        assert_eq!("sysinfo".parse::<Protocol>(), Ok(Protocol::Sysinfo));
        assert_eq!("systemd".parse::<Protocol>(), Ok(Protocol::Systemd));

        // Case insensitive
        assert_eq!("SNMP".parse::<Protocol>(), Ok(Protocol::Snmp));
        assert_eq!("Sysinfo".parse::<Protocol>(), Ok(Protocol::Sysinfo));

        // Invalid
        assert!("unknown".parse::<Protocol>().is_err());
        assert!("".parse::<Protocol>().is_err());
    }

    #[test]
    fn test_protocol_systemd_serde_roundtrip() {
        // Wire token is the lowercase `"systemd"` (via `#[serde(rename_all)]`).
        assert_eq!(
            serde_json::to_value(Protocol::Systemd).unwrap(),
            serde_json::json!("systemd")
        );
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("systemd")).unwrap(),
            Protocol::Systemd
        );
        assert_eq!(Protocol::Systemd.as_str(), "systemd");
    }

    #[test]
    fn test_value_conversions() {
        assert_eq!(TelemetryValue::from(42u64), TelemetryValue::Counter(42));
        assert_eq!(TelemetryValue::from(2.5), TelemetryValue::Gauge(2.5));
        assert_eq!(
            TelemetryValue::from("test"),
            TelemetryValue::Text("test".to_string())
        );
        assert_eq!(TelemetryValue::from(true), TelemetryValue::Boolean(true));
    }

    #[test]
    fn test_i64_conversion_preserves_precision() {
        // Positive i64 values become Counter (no f64 precision loss)
        assert_eq!(TelemetryValue::from(42i64), TelemetryValue::Counter(42));
        assert_eq!(
            TelemetryValue::from(i64::MAX),
            TelemetryValue::Counter(i64::MAX as u64)
        );

        // Negative values become Gauge (f64 is appropriate for negative values)
        assert_eq!(TelemetryValue::from(-1i64), TelemetryValue::Gauge(-1.0));

        // Zero is non-negative, becomes Counter
        assert_eq!(TelemetryValue::from(0i64), TelemetryValue::Counter(0));
    }
}
