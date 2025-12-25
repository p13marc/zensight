use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single telemetry data point emitted by bridges.
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
#[serde(untagged)]
pub enum TelemetryValue {
    /// Counter (monotonically increasing).
    Counter(u64),

    /// Gauge (can go up or down).
    Gauge(f64),

    /// Text value.
    Text(String),

    /// Boolean value.
    Boolean(bool),

    /// Binary data.
    Binary(Vec<u8>),
}

impl From<u64> for TelemetryValue {
    fn from(v: u64) -> Self {
        TelemetryValue::Counter(v)
    }
}

impl From<i64> for TelemetryValue {
    fn from(v: i64) -> Self {
        TelemetryValue::Gauge(v as f64)
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
    Syslog,
    Gnmi,
    Netflow,
    Opcua,
    Modbus,
    Sysinfo,
}

impl Protocol {
    /// Get the string representation used in key expressions.
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Snmp => "snmp",
            Protocol::Syslog => "syslog",
            Protocol::Gnmi => "gnmi",
            Protocol::Netflow => "netflow",
            Protocol::Opcua => "opcua",
            Protocol::Modbus => "modbus",
            Protocol::Sysinfo => "sysinfo",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
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
        assert_eq!(Protocol::Syslog.as_str(), "syslog");
    }

    #[test]
    fn test_value_conversions() {
        assert_eq!(TelemetryValue::from(42u64), TelemetryValue::Counter(42));
        assert_eq!(TelemetryValue::from(3.14), TelemetryValue::Gauge(3.14));
        assert_eq!(
            TelemetryValue::from("test"),
            TelemetryValue::Text("test".to_string())
        );
        assert_eq!(TelemetryValue::from(true), TelemetryValue::Boolean(true));
    }
}
