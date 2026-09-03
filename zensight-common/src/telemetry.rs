use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single telemetry data point emitted by sensors.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TelemetryPoint {
    /// Unix epoch milliseconds when the measurement was taken.
    pub timestamp: i64,

    /// Who this series belongs to.
    ///
    /// For a **proxy** sensor — snmp, gnmi, modbus — the polled device, which
    /// really is a separate machine no sensor runs on. For **every other**
    /// sensor, the reporting host, including when the point describes one of
    /// its facets: a VM guest, a container or a probe target is not a machine
    /// that publishes for itself, and filing its series under the subject
    /// puts it on no host's card at all (#883). The subject belongs in the key
    /// path and in the labels, both of which already carry it.
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

    /// Unit of the value, UCUM-style (e.g. `"By"`, `"By/s"`, `"1/s"`, `"%"`,
    /// `"s"`). Absent when the producer doesn't know. Serde-defaulted both
    /// ways, so old and new consumers interoperate over JSON and CBOR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
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
            unit: None,
        }
    }

    /// Add a label to this telemetry point.
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Set the value's unit (UCUM-style string).
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Add multiple labels to this telemetry point.
    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels.extend(labels);
        self
    }
}

/// Typed telemetry value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
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
    /// Live video / imagery streams on the opaque `@media` plane (#359). Carries
    /// no `TelemetryPoint` firehose itself — only stream *stats* (fps/kbps) ride
    /// ordinary telemetry; the pixels ride `@media/<stream>/…` as raw bytes.
    Parallax,
    /// Machine-checked desired-state assertions (#821): the sentinel pattern for
    /// what D-Bus and netlink cannot see — mounts, files, listeners, symlinks,
    /// content, permissions. Publishes alerts and one gauge
    /// (`assertions/failing`); strictly read-only, executes nothing.
    Hostspec,
    /// Out-of-band hardware health (#953) — power supplies, fans, thermal
    /// sensors and the chassis rollup, read from the BMC over Redfish. The
    /// one place a physical fault is visible when the sensors never reach
    /// hwmon, which on rack hardware is the normal case. Read-only: there is
    /// no chassis reset and no IPMI power command.
    Bmc,
    /// Proxmox VE (#818): the hypervisor as a hypervisor — guests and their
    /// `onboot`/firewall configuration, storage pools' allocated-vs-capacity,
    /// vzdump outcomes and sizes, cluster/HA/replication state. Polls the PVE
    /// API read-only with a scoped token; **no action surface at all**.
    Pve,
    /// OCI containers (#819) — the whole workload on the reference fleet, and
    /// invisible before this: per-container cgroup/OOM/restart/exit-code,
    /// image reference **and digest**, healthcheck state including the
    /// never-ran case, signature presence, and the systemd unit that owns it.
    /// Read-only socket + cgroup files; no action surface.
    Container,
    /// Outside-in synthetic checks (#820) — HTTP/TLS/DNS/TCP/ICMP against
    /// configured targets, plus local certificate files. The one sensor whose
    /// answer depends on *where it runs*: the same target from the edge, from
    /// a guest and from a workstation gives three different, equally true
    /// answers, so every result carries its vantage point.
    Probe,
    /// Durable fleet telemetry history (#898) — the first producer here that
    /// is not a sensor. It measures nothing and publishes no telemetry; it
    /// ingests everyone else's and answers range queries over it.
    ///
    /// It is a `Protocol` because the framework's identity of a producer runs
    /// through this enum: `AlertReporter::new` takes one, and `SensorRunner`
    /// derives the `sensor-budget` rule by parsing its own name as one. A
    /// history service that holds a database on a 1–2 GB VM is exactly the
    /// component that must be able to say it is approaching its budget, so
    /// "not a sensor" is not a reason to leave it outside.
    Historian,
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
            Protocol::Parallax => "parallax",
            Protocol::Hostspec => "hostspec",
            Protocol::Pve => "pve",
            Protocol::Container => "container",
            Protocol::Probe => "probe",
            Protocol::Historian => "historian",
            Protocol::Bmc => "bmc",
        }
    }

    /// Human-facing display name for the GUI. Distinct from [`as_str`](Self::as_str)
    /// (the wire/keyspace token `logs`): title-cased for the UI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Protocol::Logs => "Logs",
            Protocol::Pve => "PVE",
            Protocol::Bmc => "BMC",
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
            "parallax" => Ok(Protocol::Parallax),
            "hostspec" => Ok(Protocol::Hostspec),
            "pve" => Ok(Protocol::Pve),
            "container" => Ok(Protocol::Container),
            "probe" => Ok(Protocol::Probe),
            "historian" => Ok(Protocol::Historian),
            "bmc" => Ok(Protocol::Bmc),
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
